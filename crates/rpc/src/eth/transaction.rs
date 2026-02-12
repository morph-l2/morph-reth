//! Morph transaction conversion for `eth_` RPC responses.

use crate::MorphTransactionRequest;
use crate::types::transaction::MorphRpcTransaction;
use alloy_consensus::{
    EthereumTxEnvelope, SignableTransaction, Transaction, TxEip4844, transaction::Recovered,
};
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TxSigner;
use alloy_primitives::{Address, Bytes, Signature, TxKind, U64, U256};
use alloy_rpc_types_eth::{AccessList, Transaction as RpcTransaction, TransactionInfo};
use reth_rpc_convert::{
    SignTxRequestError, SignableTxRequest, TryIntoSimTx, TryIntoTxEnv, transaction::FromConsensusTx,
};
use reth_rpc_eth_types::EthApiError;
use revm::context::Transaction as RevmTransaction;
use std::convert::Infallible;

use morph_primitives::{MorphTxEnvelope, TxMorph};
use morph_revm::{MorphBlockEnv, MorphTxEnv};
use reth_evm::EvmEnv;

/// Converts a consensus [`MorphTxEnvelope`] to an RPC [`MorphRpcTransaction`].
impl FromConsensusTx<MorphTxEnvelope> for MorphRpcTransaction {
    type TxInfo = TransactionInfo;
    type Err = Infallible;

    fn from_consensus_tx(
        tx: MorphTxEnvelope,
        signer: Address,
        tx_info: Self::TxInfo,
    ) -> Result<Self, Self::Err> {
        let (sender, queue_index) = match &tx {
            MorphTxEnvelope::L1Msg(msg) => (Some(msg.sender), Some(U64::from(msg.queue_index))),
            _ => (None, None),
        };

        // Extract MorphTx-specific fields
        let version = tx.version();
        let fee_token_id = tx.fee_token_id().map(U64::from);
        let fee_limit = tx.fee_limit();
        let reference = tx.reference();
        let memo = tx.memo();

        let effective_gas_price = tx_info.base_fee.map(|base_fee| {
            tx.effective_tip_per_gas(base_fee)
                .unwrap_or_default()
                .saturating_add(base_fee as u128)
        });

        let inner = RpcTransaction {
            inner: Recovered::new_unchecked(tx, signer),
            block_hash: tx_info.block_hash,
            block_number: tx_info.block_number,
            transaction_index: tx_info.index,
            effective_gas_price,
        };

        Ok(Self {
            inner,
            sender,
            queue_index,
            version,
            fee_token_id,
            fee_limit,
            reference,
            memo,
        })
    }
}

/// Converts a [`MorphTransactionRequest`] into a simulated transaction envelope.
///
/// Handles both standard Ethereum transactions and Morph-specific fee token transactions.
/// All MorphTx transactions are constructed as Version 1.
impl TryIntoSimTx<MorphTxEnvelope> for MorphTransactionRequest {
    fn try_into_sim_tx(self) -> Result<MorphTxEnvelope, alloy_consensus::error::ValueError<Self>> {
        // Try to build a MorphTx; returns None if this should be a standard Ethereum tx
        let morph_tx_result = try_build_morph_tx_from_request(
            &self.inner,
            self.fee_token_id.unwrap_or_default(),
            self.fee_limit.unwrap_or_default(),
            self.reference,
            self.memo.clone(),
        );

        match morph_tx_result {
            Ok(Some(morph_tx)) => {
                let signature = Signature::new(Default::default(), Default::default(), false);
                Ok(MorphTxEnvelope::Morph(morph_tx.into_signed(signature)))
            }
            Ok(None) => {
                // Standard Ethereum transaction
                let inner = self.inner.clone();
                let envelope = inner.build_typed_simulate_transaction().map_err(|err| {
                    err.map(|inner| Self {
                        inner,
                        fee_token_id: self.fee_token_id,
                        fee_limit: self.fee_limit,
                        reference: self.reference,
                        memo: self.memo.clone(),
                    })
                })?;
                morph_envelope_from_ethereum(envelope)
                    .map_err(|err| alloy_consensus::error::ValueError::new(self, err))
            }
            Err(err) => Err(alloy_consensus::error::ValueError::new(self, err)),
        }
    }
}

/// Builds and signs a transaction from an RPC request.
///
/// Supports both standard Ethereum transactions and Morph fee token transactions.
/// All MorphTx transactions are constructed as Version 1.
impl SignableTxRequest<MorphTxEnvelope> for MorphTransactionRequest {
    async fn try_build_and_sign(
        self,
        signer: impl TxSigner<Signature> + Send,
    ) -> Result<MorphTxEnvelope, SignTxRequestError> {
        // Try to build a MorphTx; returns None if this should be a standard Ethereum tx
        let morph_tx_result = try_build_morph_tx_from_request(
            &self.inner,
            self.fee_token_id.unwrap_or_default(),
            self.fee_limit.unwrap_or_default(),
            self.reference,
            self.memo,
        );

        match morph_tx_result {
            Ok(Some(mut morph_tx)) => {
                let signature = signer.sign_transaction(&mut morph_tx).await?;
                Ok(MorphTxEnvelope::Morph(morph_tx.into_signed(signature)))
            }
            Ok(None) => {
                // Standard Ethereum transaction
                let mut tx = self
                    .inner
                    .build_typed_tx()
                    .map_err(|_| SignTxRequestError::InvalidTransactionRequest)?;
                let signature = signer.sign_transaction(&mut tx).await?;
                let signed_envelope: EthereumTxEnvelope<TxEip4844> =
                    EthereumTxEnvelope::new_unhashed(tx, signature).into();
                morph_envelope_from_ethereum(signed_envelope)
                    .map_err(|_| SignTxRequestError::InvalidTransactionRequest)
            }
            Err(_) => Err(SignTxRequestError::InvalidTransactionRequest),
        }
    }
}

/// Converts a transaction request into a transaction environment for EVM execution.
///
/// Also encodes the transaction for L1 fee calculation.
/// All MorphTx transactions are constructed as Version 1.
impl TryIntoTxEnv<MorphTxEnv, MorphBlockEnv> for MorphTransactionRequest {
    type Err = EthApiError;

    fn try_into_tx_env<Spec>(
        self,
        evm_env: &EvmEnv<Spec, MorphBlockEnv>,
    ) -> Result<MorphTxEnv, Self::Err> {
        let fee_token_id = self.fee_token_id;
        let fee_limit = self.fee_limit;
        let reference = self.reference;
        let memo = self.memo;
        let inner = self.inner;

        let inner_tx_env = inner
            .clone()
            .try_into_tx_env(evm_env)
            .map_err(EthApiError::from)?;

        let mut tx_env = MorphTxEnv::new(inner_tx_env);
        tx_env.fee_token_id = match fee_token_id {
            Some(id) => Some(
                u16::try_from(id.to::<u64>())
                    .map_err(|_| EthApiError::InvalidParams("invalid token".to_string()))?,
            ),
            None => None,
        };
        tx_env.fee_limit = fee_limit;
        tx_env.reference = reference;
        tx_env.memo = memo.clone();

        // Determine if this is a MorphTx based on Morph-specific fields
        let is_morph_tx = fee_token_id.is_some_and(|id| id.to::<u64>() > 0)
            || reference.is_some()
            || memo.as_ref().is_some_and(|m| !m.is_empty());

        if is_morph_tx {
            tx_env.inner.tx_type = morph_primitives::MORPH_TX_TYPE_ID;
            tx_env.version =
                Some(morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_1);
        }

        // RLP bytes are not generated here for eth_call and eth_estimateGas.
        // They will be generated on-demand in estimate_l1_fee if needed.
        // For real transactions, RLP bytes are extracted in FromRecoveredTx.
        tx_env.rlp_bytes = None;
        Ok(tx_env)
    }
}

/// Converts an Ethereum transaction envelope to a Morph envelope.
///
/// EIP-4844 blob transactions are not supported on Morph.
fn morph_envelope_from_ethereum(
    env: EthereumTxEnvelope<TxEip4844>,
) -> Result<MorphTxEnvelope, &'static str> {
    match env {
        EthereumTxEnvelope::Legacy(tx) => Ok(MorphTxEnvelope::Legacy(tx)),
        EthereumTxEnvelope::Eip2930(tx) => Ok(MorphTxEnvelope::Eip2930(tx)),
        EthereumTxEnvelope::Eip1559(tx) => Ok(MorphTxEnvelope::Eip1559(tx)),
        EthereumTxEnvelope::Eip7702(tx) => Ok(MorphTxEnvelope::Eip7702(tx)),
        EthereumTxEnvelope::Eip4844(_) => Err("EIP-4844 transactions are not supported on Morph"),
    }
}

/// Attempts to build a [`TxMorph`] from an RPC transaction request.
///
/// Returns `Ok(Some(tx))` if a MorphTx should be constructed (always Version 1),
/// `Ok(None)` if this should be a standard Ethereum transaction,
/// or `Err(...)` if there's a validation error.
///
/// A MorphTx is constructed when any of these conditions are met:
/// - `feeTokenID > 0` (ERC20 gas payment)
/// - `reference` is present
/// - `memo` is present and non-empty
fn try_build_morph_tx_from_request(
    req: &alloy_rpc_types_eth::TransactionRequest,
    fee_token_id: U64,
    fee_limit: U256,
    reference: Option<alloy_primitives::B256>,
    memo: Option<alloy_primitives::Bytes>,
) -> Result<Option<TxMorph>, &'static str> {
    let fee_token_id_u16 = u16::try_from(fee_token_id.to::<u64>()).map_err(|_| "invalid token")?;

    // Check if this should be a MorphTx
    let has_fee_token = fee_token_id_u16 > 0;
    let has_reference = reference.is_some();
    let has_memo = memo.as_ref().is_some_and(|m| !m.is_empty());

    if !has_fee_token && !has_reference && !has_memo {
        // No Morph-specific fields → standard Ethereum tx
        return Ok(None);
    }

    // All MorphTx are constructed as Version 1
    let version = morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_1;

    // Now build the MorphTx
    let chain_id = req
        .chain_id
        .ok_or("missing chain_id for morph transaction")?;
    let gas_limit = req.gas.unwrap_or_default() as u128;
    let nonce = req.nonce.unwrap_or_default();
    let max_fee_per_gas = req.max_fee_per_gas.or(req.gas_price).unwrap_or_default();
    let max_priority_fee_per_gas = req.max_priority_fee_per_gas.unwrap_or_default();
    let access_list: AccessList = req.access_list.clone().unwrap_or_default();
    let input = req.input.clone().into_input().unwrap_or_default();
    let to = req.to.unwrap_or(TxKind::Create);

    Ok(Some(TxMorph {
        chain_id,
        nonce,
        gas_limit,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        to,
        value: req.value.unwrap_or_default(),
        access_list,
        input,
        fee_token_id: fee_token_id_u16,
        fee_limit,
        version,
        reference,
        memo,
    }))
}

/// Attempts to build a [`TxMorph`] from an existing transaction environment.
///
/// Returns `Ok(Some(tx))` if a MorphTx should be constructed (always Version 1),
/// `Ok(None)` if this should be a standard Ethereum transaction,
/// or `Err(...)` if there's a validation error.
///
/// A MorphTx is constructed when any of these conditions are met:
/// - `feeTokenID > 0` (ERC20 gas payment)
/// - `reference` is present
/// - `memo` is present and non-empty
fn try_build_morph_tx_from_env<Spec>(
    tx_env: &MorphTxEnv,
    fee_token_id: U64,
    fee_limit: U256,
    access_list: AccessList,
    evm_env: &EvmEnv<Spec, MorphBlockEnv>,
    reference: Option<alloy_primitives::B256>,
    memo: Option<alloy_primitives::Bytes>,
) -> Result<Option<TxMorph>, EthApiError> {
    let fee_token_id_u16 = u16::try_from(fee_token_id.to::<u64>())
        .map_err(|_| EthApiError::InvalidParams("invalid token".to_string()))?;

    // Check if this should be a MorphTx
    let has_fee_token = fee_token_id_u16 > 0;
    let has_reference = reference.is_some();
    let has_memo = memo.as_ref().is_some_and(|m| !m.is_empty());

    if !has_fee_token && !has_reference && !has_memo {
        // No Morph-specific fields → standard Ethereum tx
        return Ok(None);
    }

    // All MorphTx are constructed as Version 1
    let version = morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_1;

    let chain_id = tx_env.chain_id().unwrap_or(evm_env.cfg_env.chain_id);
    let input = tx_env.input().clone();
    let to = tx_env.kind();
    let max_fee_per_gas = tx_env.max_fee_per_gas();
    let max_priority_fee_per_gas = tx_env.max_priority_fee_per_gas().unwrap_or_default();

    Ok(Some(TxMorph {
        chain_id,
        nonce: tx_env.nonce(),
        gas_limit: tx_env.gas_limit() as u128,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        to,
        value: tx_env.value(),
        access_list,
        input,
        fee_token_id: fee_token_id_u16,
        fee_limit,
        version,
        reference,
        memo,
    }))
}

/// Builds a transaction with mock signature for L1 fee estimation.
///
/// This is used for eth_call and eth_estimateGas where we don't have
/// a real signature, but need to estimate the RLP-encoded transaction size
/// for L1 data fee calculation.
///
/// The mock signature has a fixed size (65 bytes for ECDSA), which is
/// sufficient for accurate L1 fee estimation since the fee is based on
/// the calldata size.
pub fn build_tx_with_mock_signature<Spec>(
    tx_env: &MorphTxEnv,
    evm_env: &EvmEnv<Spec, MorphBlockEnv>,
) -> Result<Bytes, EthApiError> {
    let access_list = tx_env.access_list.clone();
    let fee_token_id = U64::from(tx_env.fee_token_id.unwrap_or_default());
    let fee_limit = tx_env.fee_limit.unwrap_or_default();
    let reference = tx_env.reference;
    let memo = tx_env.memo.clone();

    // Try to build a MorphTx; returns None if this should be a standard Ethereum tx
    match try_build_morph_tx_from_env(
        tx_env,
        fee_token_id,
        fee_limit,
        access_list.clone(),
        evm_env,
        reference,
        memo,
    )? {
        Some(morph_tx) => Ok(encode_2718(morph_tx)),
        None => {
            // Build an EIP-1559 transaction directly from tx_env for L1 fee calculation.
            // This avoids requiring a complete TransactionRequest (which eth_call doesn't provide).
            let tx = alloy_consensus::TxEip1559 {
                chain_id: tx_env.chain_id().unwrap_or(evm_env.cfg_env.chain_id),
                nonce: tx_env.nonce(),
                gas_limit: tx_env.gas_limit(),
                max_fee_per_gas: tx_env.max_fee_per_gas(),
                max_priority_fee_per_gas: tx_env.max_priority_fee_per_gas().unwrap_or_default(),
                to: tx_env.kind(),
                value: tx_env.value(),
                access_list,
                input: tx_env.input().clone(),
            };
            // Use a mock signature (all zeros) for encoding.
            // Only the signature size matters for L1 fee calculation, not its validity.
            let signature = Signature::new(Default::default(), Default::default(), false);
            let signed = tx.into_signed(signature);
            let envelope: EthereumTxEnvelope<alloy_consensus::TxEip4844> =
                EthereumTxEnvelope::Eip1559(signed);
            Ok(encode_2718(envelope))
        }
    }
}

/// Encodes a transaction using EIP-2718 typed transaction encoding.
fn encode_2718<T: Encodable2718>(tx: T) -> Bytes {
    let mut out = Vec::with_capacity(tx.encode_2718_len());
    tx.encode_2718(&mut out);
    Bytes::from(out)
}
