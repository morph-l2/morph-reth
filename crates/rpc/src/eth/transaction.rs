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
        let fee_token_id = tx.fee_token_id().map(U64::from);
        let fee_limit = tx.fee_limit();

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
            fee_token_id,
            fee_limit,
        })
    }
}

/// Converts a [`MorphTransactionRequest`] into a simulated transaction envelope.
///
/// Handles both standard Ethereum transactions and Morph-specific fee token transactions.
impl TryIntoSimTx<MorphTxEnvelope> for MorphTransactionRequest {
    fn try_into_sim_tx(self) -> Result<MorphTxEnvelope, alloy_consensus::error::ValueError<Self>> {
        if let Some(fee_token_id) = self.fee_token_id.filter(|id| id.to::<u64>() > 0) {
            let reference = self.reference;
            let memo = self.memo.clone();
            let morph_tx = build_morph_tx_from_request(
                &self.inner,
                fee_token_id,
                self.fee_limit.unwrap_or_default(),
                reference,
                memo,
            )
            .map_err(|err| alloy_consensus::error::ValueError::new(self, err))?;
            let signature = Signature::new(Default::default(), Default::default(), false);
            return Ok(MorphTxEnvelope::Morph(morph_tx.into_signed(signature)));
        }

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
}

/// Builds and signs a transaction from an RPC request.
///
/// Supports both standard Ethereum transactions and Morph fee token transactions.
impl SignableTxRequest<MorphTxEnvelope> for MorphTransactionRequest {
    async fn try_build_and_sign(
        self,
        signer: impl TxSigner<Signature> + Send,
    ) -> Result<MorphTxEnvelope, SignTxRequestError> {
        if let Some(fee_token_id) = self.fee_token_id.filter(|id| id.to::<u64>() > 0) {
            let mut morph_tx = build_morph_tx_from_request(
                &self.inner,
                fee_token_id,
                self.fee_limit.unwrap_or_default(),
                self.reference,
                self.memo,
            )
            .map_err(|_| SignTxRequestError::InvalidTransactionRequest)?;
            let signature = signer.sign_transaction(&mut morph_tx).await?;
            return Ok(MorphTxEnvelope::Morph(morph_tx.into_signed(signature)));
        }

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
}

/// Converts a transaction request into a transaction environment for EVM execution.
///
/// Also encodes the transaction for L1 fee calculation.
impl TryIntoTxEnv<MorphTxEnv, MorphBlockEnv> for MorphTransactionRequest {
    type Err = EthApiError;

    fn try_into_tx_env<Spec>(
        self,
        evm_env: &EvmEnv<Spec, MorphBlockEnv>,
    ) -> Result<MorphTxEnv, Self::Err> {
        let fee_token_id = self.fee_token_id;
        let fee_limit = self.fee_limit;
        let inner = self.inner;
        let access_list = inner.access_list.clone().unwrap_or_default();

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
        if tx_env.fee_token_id.unwrap_or_default() > 0 {
            tx_env.inner.tx_type = morph_primitives::MORPH_TX_TYPE_ID;
        }

        let rlp_bytes = encode_tx_for_l1_fee(&tx_env, access_list, evm_env, inner)?;

        tx_env.rlp_bytes = Some(rlp_bytes);
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

/// Builds a [`TxMorph`] from an RPC transaction request.
///
/// Extracts fields from the request and constructs a Morph transaction
/// with the specified fee token ID, fee limit, reference, and memo.
fn build_morph_tx_from_request(
    req: &alloy_rpc_types_eth::TransactionRequest,
    fee_token_id: U64,
    fee_limit: U256,
    reference: Option<alloy_primitives::B256>,
    memo: Option<alloy_primitives::Bytes>,
) -> Result<TxMorph, &'static str> {
    let chain_id = req
        .chain_id
        .ok_or("missing chain_id for morph transaction")?;
    let fee_token_id = u16::try_from(fee_token_id.to::<u64>()).map_err(|_| "invalid token")?;
    let gas_limit = req.gas.unwrap_or_default() as u128;
    let nonce = req.nonce.unwrap_or_default();
    let max_fee_per_gas = req.max_fee_per_gas.or(req.gas_price).unwrap_or_default();
    let max_priority_fee_per_gas = req.max_priority_fee_per_gas.unwrap_or_default();
    let access_list: AccessList = req.access_list.clone().unwrap_or_default();
    let input = req.input.clone().into_input().unwrap_or_default();
    let to = req.to.unwrap_or(TxKind::Create);

    // Determine version based on presence of reference or memo
    let version = if reference.is_some() || memo.is_some() {
        morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_1
    } else {
        morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_0
    };

    Ok(TxMorph {
        chain_id,
        nonce,
        gas_limit,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        to,
        value: req.value.unwrap_or_default(),
        access_list,
        input,
        fee_token_id,
        fee_limit,
        version,
        reference,
        memo,
    })
}

/// Builds a [`TxMorph`] from an existing transaction environment.
///
/// Used for encoding transactions for L1 fee calculation.
/// Extracts reference and memo from the transaction environment if present.
fn build_morph_tx_from_env<Spec>(
    tx_env: &MorphTxEnv,
    fee_token_id: U64,
    fee_limit: U256,
    access_list: AccessList,
    evm_env: &EvmEnv<Spec, MorphBlockEnv>,
    reference: Option<alloy_primitives::B256>,
    memo: Option<alloy_primitives::Bytes>,
) -> Result<TxMorph, EthApiError> {
    let fee_token_id = u16::try_from(fee_token_id.to::<u64>())
        .map_err(|_| EthApiError::InvalidParams("invalid token".to_string()))?;
    let chain_id = tx_env.chain_id().unwrap_or(evm_env.cfg_env.chain_id);
    let input = tx_env.input().clone();
    let to = tx_env.kind();
    let max_fee_per_gas = tx_env.max_fee_per_gas();
    let max_priority_fee_per_gas = tx_env.max_priority_fee_per_gas().unwrap_or_default();

    // Determine version based on presence of reference or memo
    let version = if reference.is_some() || memo.is_some() {
        morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_1
    } else {
        morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_0
    };

    Ok(TxMorph {
        chain_id,
        nonce: tx_env.nonce(),
        gas_limit: tx_env.gas_limit() as u128,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        to,
        value: tx_env.value(),
        access_list,
        input,
        fee_token_id,
        fee_limit,
        version,
        reference,
        memo,
    })
}

/// Encodes a transaction for L1 fee calculation.
///
/// Returns the RLP-encoded bytes used to calculate the L1 data fee.
fn encode_tx_for_l1_fee<Spec>(
    tx_env: &MorphTxEnv,
    access_list: AccessList,
    evm_env: &EvmEnv<Spec, MorphBlockEnv>,
    inner: alloy_rpc_types_eth::TransactionRequest,
) -> Result<Bytes, EthApiError> {
    if tx_env.fee_token_id.unwrap_or_default() > 0 {
        let fee_token_id = U64::from(tx_env.fee_token_id.unwrap_or_default());
        let fee_limit = tx_env.fee_limit.unwrap_or_default();
        let reference = tx_env.reference;
        let memo = tx_env.memo.clone();
        let morph_tx = build_morph_tx_from_env(
            tx_env,
            fee_token_id,
            fee_limit,
            access_list,
            evm_env,
            reference,
            memo,
        )?;
        Ok(encode_2718(morph_tx))
    } else {
        let envelope = inner
            .build_typed_simulate_transaction()
            .map_err(|err| EthApiError::InvalidParams(err.to_string()))?;
        Ok(encode_2718(envelope))
    }
}

/// Encodes a transaction using EIP-2718 typed transaction encoding.
fn encode_2718<T: Encodable2718>(tx: T) -> Bytes {
    let mut out = Vec::with_capacity(tx.encode_2718_len());
    tx.encode_2718(&mut out);
    Bytes::from(out)
}
