//! Morph transaction conversion for `eth_` RPC responses.

use crate::MorphTransactionRequest;
use crate::types::transaction::MorphRpcTransaction;
use alloy_consensus::{
    EthereumTxEnvelope, SignableTransaction, Transaction, TxEip4844, transaction::Recovered,
};
use alloy_network::TxSigner;
use alloy_primitives::{Address, Signature, TxKind, U64, U256};
use alloy_rpc_types_eth::{AccessList, Transaction as RpcTransaction, TransactionInfo};
use reth_rpc_convert::{
    SignTxRequestError, SignableTxRequest, TryIntoSimTx, TryIntoTxEnv, transaction::FromConsensusTx,
};
use reth_rpc_eth_types::EthApiError;
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
        let memo = tx.memo().cloned();

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

        let inner_tx_env = inner.try_into_tx_env(evm_env).map_err(EthApiError::from)?;

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

        // L1 fee handling for different RPC methods:
        //
        // 1. eth_estimateGas (disable_fee_charge = false):
        //    - Must calculate L1 data fee to check if sender has sufficient balance
        //    - Matches go-ethereum behavior: available.Sub(available, l1DataFee)
        //    - Generate RLP bytes for L1 fee calculation
        //
        // 2. eth_call (disable_fee_charge = true):
        //    - Pure EVM simulation, no fee deduction or balance check
        //    - Matches go-ethereum behavior: ApplyMessage(..., l1Fee = 0)
        //    - Skip RLP encoding to avoid L1 fee calculation
        //
        // The handler layer (validate_and_deduct_eth_fee) will:
        // - Calculate L1 fee based on rlp_bytes (None → empty slice → fee = 0)
        // - Skip balance check when disable_fee_charge = true
        if !evm_env.cfg_env.disable_fee_charge {
            // eth_estimateGas: encode transaction for L1 fee calculation
            tx_env.rlp_bytes = Some(tx_env.encode_for_l1_fee(evm_env.cfg_env.chain_id));
        } else {
            // eth_call: skip L1 fee by not providing RLP bytes
            tx_env.rlp_bytes = None;
        }

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
    let gas_limit = req.gas.unwrap_or_default();
    let nonce = req.nonce.unwrap_or_default();
    let max_fee_per_gas = req.max_fee_per_gas.or(req.gas_price).unwrap_or_default();
    let max_priority_fee_per_gas = req.max_priority_fee_per_gas.unwrap_or_default();
    let access_list: AccessList = req.access_list.clone().unwrap_or_default();
    let input = req.input.clone().into_input().unwrap_or_default();
    let to = req.to.unwrap_or(TxKind::Create);

    let morph_tx = TxMorph {
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
    };

    // Validate all MorphTx constraints: version-specific rules, gas fee ordering,
    // and memo length. This catches invalid combinations early at the RPC layer.
    morph_tx.validate()?;

    Ok(Some(morph_tx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, Bytes, address};
    use alloy_rpc_types_eth::TransactionRequest;
    use morph_chainspec::MorphHardfork;
    use revm::context::{BlockEnv, CfgEnv};

    /// Helper function to create a basic TransactionRequest for testing
    fn create_basic_transaction_request() -> TransactionRequest {
        TransactionRequest {
            from: Some(address!("0000000000000000000000000000000000000001")),
            to: Some(address!("0000000000000000000000000000000000000002").into()),
            gas: Some(100000),
            gas_price: Some(1000000000),
            value: Some(U256::from(1000)),
            nonce: Some(1),
            chain_id: Some(2818),
            ..Default::default()
        }
    }

    /// Helper function to create a basic EvmEnv for testing
    fn create_evm_env(disable_fee_charge: bool) -> EvmEnv<MorphHardfork, MorphBlockEnv> {
        let mut cfg = CfgEnv::<MorphHardfork>::default();
        cfg.disable_fee_charge = disable_fee_charge;
        cfg.chain_id = 2818;

        // Construct MorphBlockEnv directly to avoid clippy warning
        let block_env = MorphBlockEnv {
            inner: BlockEnv {
                number: alloy_primitives::U256::from(1),
                beneficiary: alloy_primitives::Address::ZERO,
                timestamp: alloy_primitives::U256::from(1234567890),
                gas_limit: 30000000u64,
                basefee: 1000000000u64,
                difficulty: alloy_primitives::U256::ZERO,
                prevrandao: Some(B256::ZERO),
                blob_excess_gas_and_price: None,
            },
        };

        EvmEnv::new(cfg, block_env)
    }

    /// Test that eth_call (disable_fee_charge = true) skips RLP encoding for L1 fee calculation.
    ///
    /// This ensures that eth_call does not calculate L1 data fee, matching go-ethereum behavior
    /// where ApplyMessage is called with l1Fee = 0.
    #[test]
    fn test_eth_call_skips_l1_fee_encoding() {
        // Arrange: Create a standard Ethereum transaction request
        let request = MorphTransactionRequest {
            inner: create_basic_transaction_request(),
            fee_token_id: None,
            fee_limit: None,
            reference: None,
            memo: None,
        };

        // eth_call scenario: disable_fee_charge = true
        let evm_env = create_evm_env(true);

        // Act: Convert to TxEnv
        let tx_env = request
            .try_into_tx_env(&evm_env)
            .expect("conversion should succeed");

        // Assert: rlp_bytes should be None (no L1 fee encoding)
        assert!(
            tx_env.rlp_bytes.is_none(),
            "eth_call should not encode RLP bytes for L1 fee calculation"
        );
    }

    /// Test that eth_estimateGas (disable_fee_charge = false) generates RLP encoding for L1 fee
    /// calculation.
    ///
    /// This ensures that eth_estimateGas correctly calculates L1 data fee, matching go-ethereum
    /// behavior where available balance is reduced by l1DataFee before checking sufficiency.
    #[test]
    fn test_eth_estimate_gas_encodes_for_l1_fee() {
        // Arrange: Create a standard Ethereum transaction request
        let request = MorphTransactionRequest {
            inner: create_basic_transaction_request(),
            fee_token_id: None,
            fee_limit: None,
            reference: None,
            memo: None,
        };

        // eth_estimateGas scenario: disable_fee_charge = false (default)
        let evm_env = create_evm_env(false);

        // Act: Convert to TxEnv
        let tx_env = request
            .try_into_tx_env(&evm_env)
            .expect("conversion should succeed");

        // Assert: rlp_bytes should exist and not be empty
        assert!(
            tx_env.rlp_bytes.is_some(),
            "eth_estimateGas should encode RLP bytes for L1 fee calculation"
        );
        assert!(
            !tx_env.rlp_bytes.unwrap().is_empty(),
            "RLP bytes should not be empty"
        );
    }

    /// Test that MorphTx encoding includes all Morph-specific fields when disable_fee_charge is
    /// false.
    ///
    /// This verifies that:
    /// 1. MorphTx transactions are correctly detected based on fee_token_id, reference, or memo
    /// 2. The transaction type is set to MORPH_TX_TYPE_ID (0x7F)
    /// 3. All Morph-specific fields are properly set in the TxEnv
    /// 4. RLP encoding is generated for L1 fee calculation
    #[test]
    fn test_morph_tx_encoding_includes_all_fields() {
        // Arrange: Create a MorphTx with all special fields
        let reference = B256::random();
        let memo = Bytes::from("test memo");

        let request = MorphTransactionRequest {
            inner: create_basic_transaction_request(),
            fee_token_id: Some(U64::from(1)), // Triggers MorphTx (use U64, not U256)
            fee_limit: Some(U256::from(1000000)),
            reference: Some(reference),
            memo: Some(memo.clone()),
        };

        // eth_estimateGas scenario: should encode for L1 fee
        let evm_env = create_evm_env(false);

        // Act: Convert to TxEnv
        let tx_env = request
            .try_into_tx_env(&evm_env)
            .expect("conversion should succeed");

        // Assert: RLP bytes should be generated
        assert!(
            tx_env.rlp_bytes.is_some(),
            "MorphTx should be encoded for L1 fee calculation"
        );

        // Assert: Transaction type should be MorphTx (0x7F)
        assert_eq!(
            tx_env.inner.tx_type,
            morph_primitives::MORPH_TX_TYPE_ID,
            "Transaction type should be MorphTx (0x7F)"
        );

        // Assert: MorphTx-specific fields should be correctly set
        assert_eq!(
            tx_env.fee_token_id,
            Some(1),
            "fee_token_id should be set correctly"
        );
        assert_eq!(
            tx_env.fee_limit,
            Some(U256::from(1000000)),
            "fee_limit should be set correctly"
        );
        assert_eq!(
            tx_env.reference,
            Some(reference),
            "reference should be set correctly"
        );
        assert_eq!(tx_env.memo, Some(memo), "memo should be set correctly");

        // Assert: Version should be set to MORPH_TX_VERSION_1
        assert_eq!(
            tx_env.version,
            Some(morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_1),
            "version should be set to MORPH_TX_VERSION_1"
        );
    }

    /// Test that eth_call with MorphTx still skips RLP encoding.
    ///
    /// Even though it's a MorphTx, eth_call should not encode for L1 fee.
    #[test]
    fn test_eth_call_with_morph_tx_skips_encoding() {
        // Arrange: Create a MorphTx
        let request = MorphTransactionRequest {
            inner: create_basic_transaction_request(),
            fee_token_id: Some(U64::from(1)), // Use U64, not U256
            fee_limit: Some(U256::from(1000000)),
            reference: Some(B256::random()),
            memo: Some(Bytes::from("test")),
        };

        // eth_call scenario: disable_fee_charge = true
        let evm_env = create_evm_env(true);

        // Act: Convert to TxEnv
        let tx_env = request
            .try_into_tx_env(&evm_env)
            .expect("conversion should succeed");

        // Assert: Even for MorphTx, eth_call should not encode
        assert!(
            tx_env.rlp_bytes.is_none(),
            "eth_call should not encode RLP bytes even for MorphTx"
        );

        // Assert: Transaction type should still be MorphTx
        assert_eq!(
            tx_env.inner.tx_type,
            morph_primitives::MORPH_TX_TYPE_ID,
            "Transaction type should still be MorphTx"
        );
    }

    /// Test that standard Ethereum transactions (non-MorphTx) are handled correctly.
    ///
    /// This verifies that when no Morph-specific fields are present, the transaction
    /// is treated as a standard Ethereum transaction.
    #[test]
    fn test_standard_ethereum_tx_encoding() {
        // Arrange: Create a standard Ethereum transaction (no Morph fields)
        let request = MorphTransactionRequest {
            inner: create_basic_transaction_request(),
            fee_token_id: None,
            fee_limit: None,
            reference: None,
            memo: None,
        };

        // eth_estimateGas scenario
        let evm_env = create_evm_env(false);

        // Act: Convert to TxEnv
        let tx_env = request
            .try_into_tx_env(&evm_env)
            .expect("conversion should succeed");

        // Assert: RLP bytes should be generated
        assert!(tx_env.rlp_bytes.is_some(), "Standard tx should be encoded");

        // Assert: Transaction type should NOT be MorphTx
        assert_ne!(
            tx_env.inner.tx_type,
            morph_primitives::MORPH_TX_TYPE_ID,
            "Transaction type should not be MorphTx for standard Ethereum tx"
        );

        // Assert: Morph-specific fields should be None
        assert!(tx_env.fee_token_id.is_none());
        assert!(tx_env.fee_limit.is_none());
        assert!(tx_env.reference.is_none());
        assert!(tx_env.memo.is_none());
        assert!(tx_env.version.is_none());
    }

    // =========================================================================
    // morph_envelope_from_ethereum tests
    // =========================================================================

    #[test]
    fn morph_envelope_from_ethereum_legacy() {
        use alloy_consensus::{Signed, TxLegacy};
        let signed = Signed::new_unchecked(
            TxLegacy {
                gas_limit: 21_000,
                ..Default::default()
            },
            Signature::new(U256::ZERO, U256::ZERO, false),
            Default::default(),
        );
        let eth = EthereumTxEnvelope::Legacy(signed);
        let result = morph_envelope_from_ethereum(eth);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), MorphTxEnvelope::Legacy(_)));
    }

    #[test]
    fn morph_envelope_from_ethereum_eip2930() {
        use alloy_consensus::{Signed, TxEip2930};
        let signed = Signed::new_unchecked(
            TxEip2930 {
                gas_limit: 21_000,
                ..Default::default()
            },
            Signature::new(U256::ZERO, U256::ZERO, false),
            Default::default(),
        );
        let eth = EthereumTxEnvelope::Eip2930(signed);
        let result = morph_envelope_from_ethereum(eth);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), MorphTxEnvelope::Eip2930(_)));
    }

    #[test]
    fn morph_envelope_from_ethereum_eip1559() {
        use alloy_consensus::{Signed, TxEip1559};
        let signed = Signed::new_unchecked(
            TxEip1559 {
                gas_limit: 21_000,
                ..Default::default()
            },
            Signature::new(U256::ZERO, U256::ZERO, false),
            Default::default(),
        );
        let eth = EthereumTxEnvelope::Eip1559(signed);
        let result = morph_envelope_from_ethereum(eth);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), MorphTxEnvelope::Eip1559(_)));
    }

    #[test]
    fn morph_envelope_from_ethereum_eip7702() {
        use alloy_consensus::{Signed, TxEip7702};
        let signed = Signed::new_unchecked(
            TxEip7702 {
                gas_limit: 21_000,
                ..Default::default()
            },
            Signature::new(U256::ZERO, U256::ZERO, false),
            Default::default(),
        );
        let eth = EthereumTxEnvelope::Eip7702(signed);
        let result = morph_envelope_from_ethereum(eth);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), MorphTxEnvelope::Eip7702(_)));
    }

    #[test]
    fn morph_envelope_from_ethereum_rejects_eip4844() {
        use alloy_consensus::Signed;
        let signed = Signed::new_unchecked(
            TxEip4844 {
                gas_limit: 21_000,
                ..Default::default()
            },
            Signature::new(U256::ZERO, U256::ZERO, false),
            Default::default(),
        );
        let eth = EthereumTxEnvelope::Eip4844(signed);
        let result = morph_envelope_from_ethereum(eth);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("EIP-4844"));
    }

    // =========================================================================
    // try_build_morph_tx_from_request tests
    // =========================================================================

    #[test]
    fn try_build_morph_tx_returns_none_for_standard_tx() {
        let req = create_basic_transaction_request();
        let result = try_build_morph_tx_from_request(&req, U64::ZERO, U256::ZERO, None, None);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn try_build_morph_tx_with_fee_token_id() {
        let req = create_basic_transaction_request();
        let result =
            try_build_morph_tx_from_request(&req, U64::from(1), U256::from(1_000_000), None, None);
        assert!(result.is_ok());
        let tx = result.unwrap().unwrap();
        assert_eq!(tx.fee_token_id, 1);
        assert_eq!(tx.fee_limit, U256::from(1_000_000));
        assert_eq!(
            tx.version,
            morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_1
        );
    }

    #[test]
    fn try_build_morph_tx_with_reference_only() {
        let req = create_basic_transaction_request();
        let reference = B256::random();
        let result =
            try_build_morph_tx_from_request(&req, U64::ZERO, U256::ZERO, Some(reference), None);
        assert!(result.is_ok());
        let tx = result.unwrap().unwrap();
        assert_eq!(tx.reference, Some(reference));
        assert_eq!(tx.fee_token_id, 0);
    }

    #[test]
    fn try_build_morph_tx_with_memo_only() {
        let req = create_basic_transaction_request();
        let memo = Bytes::from("hello world");
        let result =
            try_build_morph_tx_from_request(&req, U64::ZERO, U256::ZERO, None, Some(memo.clone()));
        assert!(result.is_ok());
        let tx = result.unwrap().unwrap();
        assert_eq!(tx.memo, Some(memo));
    }

    #[test]
    fn try_build_morph_tx_empty_memo_is_not_trigger() {
        let req = create_basic_transaction_request();
        let result =
            try_build_morph_tx_from_request(&req, U64::ZERO, U256::ZERO, None, Some(Bytes::new()));
        assert!(result.is_ok());
        // Empty memo should NOT trigger MorphTx creation
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn try_build_morph_tx_requires_chain_id() {
        let mut req = create_basic_transaction_request();
        req.chain_id = None;
        let result =
            try_build_morph_tx_from_request(&req, U64::from(1), U256::from(100), None, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("chain_id"));
    }

    #[test]
    fn try_build_morph_tx_sets_correct_tx_fields() {
        let req = create_basic_transaction_request();
        let result = try_build_morph_tx_from_request(
            &req,
            U64::from(2),
            U256::from(500_000),
            Some(B256::random()),
            Some(Bytes::from("memo")),
        );
        let tx = result.unwrap().unwrap();
        assert_eq!(tx.chain_id, 2818);
        assert_eq!(tx.gas_limit, 100000);
        assert_eq!(tx.nonce, 1);
        assert_eq!(tx.max_fee_per_gas, 1_000_000_000); // falls back to gas_price
        assert_eq!(tx.value, U256::from(1000));
    }

    // =========================================================================
    // FromConsensusTx tests
    // =========================================================================

    #[test]
    fn from_consensus_tx_l1_message() {
        use alloy_primitives::Sealed;
        use morph_primitives::TxL1Msg;

        let l1_msg = TxL1Msg {
            queue_index: 42,
            gas_limit: 100_000,
            sender: address!("000000000000000000000000000000000000dead"),
            ..Default::default()
        };
        let tx = MorphTxEnvelope::L1Msg(Sealed::new_unchecked(l1_msg, B256::default()));
        let signer = Address::ZERO;
        let tx_info = TransactionInfo {
            hash: Some(B256::ZERO),
            block_hash: Some(B256::random()),
            block_number: Some(10),
            index: Some(0),
            base_fee: Some(1000),
        };

        let rpc_tx = MorphRpcTransaction::from_consensus_tx(tx, signer, tx_info).unwrap();
        assert_eq!(
            rpc_tx.sender,
            Some(address!("000000000000000000000000000000000000dead"))
        );
        assert_eq!(rpc_tx.queue_index, Some(U64::from(42)));
        // L1 messages don't have MorphTx-specific fields
        assert!(rpc_tx.version.is_none());
        assert!(rpc_tx.fee_token_id.is_none());
    }

    #[test]
    fn from_consensus_tx_morph_tx() {
        use alloy_consensus::Signed;

        let morph_tx = TxMorph {
            chain_id: 2818,
            nonce: 5,
            gas_limit: 50_000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000,
            fee_token_id: 3,
            fee_limit: U256::from(100_000),
            version: morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_1,
            reference: Some(B256::random()),
            memo: Some(Bytes::from("hello")),
            ..Default::default()
        };
        let tx = MorphTxEnvelope::Morph(Signed::new_unchecked(
            morph_tx,
            Signature::new(U256::ZERO, U256::ZERO, false),
            Default::default(),
        ));
        let signer = address!("0000000000000000000000000000000000000099");
        let tx_info = TransactionInfo {
            hash: Some(B256::ZERO),
            block_hash: Some(B256::random()),
            block_number: Some(100),
            index: Some(5),
            base_fee: Some(1_000_000_000),
        };

        let rpc_tx = MorphRpcTransaction::from_consensus_tx(tx, signer, tx_info).unwrap();
        // MorphTx should NOT have L1 message fields
        assert!(rpc_tx.sender.is_none());
        assert!(rpc_tx.queue_index.is_none());
        // Should have MorphTx-specific fields
        assert_eq!(
            rpc_tx.version,
            Some(morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_1)
        );
        assert_eq!(rpc_tx.fee_token_id, Some(U64::from(3)));
        assert_eq!(rpc_tx.fee_limit, Some(U256::from(100_000)));
        assert!(rpc_tx.reference.is_some());
        assert_eq!(rpc_tx.memo, Some(Bytes::from("hello")));
    }

    #[test]
    fn from_consensus_tx_standard_eip1559() {
        use alloy_consensus::{Signed, TxEip1559};

        let tx = MorphTxEnvelope::Eip1559(Signed::new_unchecked(
            TxEip1559 {
                chain_id: 2818,
                nonce: 0,
                gas_limit: 21_000,
                max_fee_per_gas: 2_000_000_000,
                max_priority_fee_per_gas: 1_000_000,
                ..Default::default()
            },
            Signature::new(U256::ZERO, U256::ZERO, false),
            Default::default(),
        ));
        let signer = address!("0000000000000000000000000000000000000001");
        let tx_info = TransactionInfo {
            hash: Some(B256::ZERO),
            block_hash: None,
            block_number: None,
            index: None,
            base_fee: Some(1_000_000_000),
        };

        let rpc_tx = MorphRpcTransaction::from_consensus_tx(tx, signer, tx_info).unwrap();
        // Standard tx should have no L1 message or MorphTx fields
        assert!(rpc_tx.sender.is_none());
        assert!(rpc_tx.queue_index.is_none());
        assert!(rpc_tx.version.is_none());
        assert!(rpc_tx.fee_token_id.is_none());
        assert!(rpc_tx.fee_limit.is_none());
        assert!(rpc_tx.reference.is_none());
        assert!(rpc_tx.memo.is_none());
    }

    #[test]
    fn from_consensus_tx_effective_gas_price_calculated() {
        use alloy_consensus::{Signed, TxEip1559};

        let tx = MorphTxEnvelope::Eip1559(Signed::new_unchecked(
            TxEip1559 {
                chain_id: 2818,
                gas_limit: 21_000,
                max_fee_per_gas: 3_000_000_000,
                max_priority_fee_per_gas: 500_000_000,
                ..Default::default()
            },
            Signature::new(U256::ZERO, U256::ZERO, false),
            Default::default(),
        ));
        let base_fee = 1_000_000_000u64;
        let tx_info = TransactionInfo {
            hash: Some(B256::ZERO),
            block_hash: Some(B256::random()),
            block_number: Some(1),
            index: Some(0),
            base_fee: Some(base_fee),
        };

        let rpc_tx = MorphRpcTransaction::from_consensus_tx(tx, Address::ZERO, tx_info).unwrap();
        // effective_gas_price = min(max_priority_fee, max_fee - base_fee) + base_fee
        // = min(500_000_000, 3_000_000_000 - 1_000_000_000) + 1_000_000_000
        // = 500_000_000 + 1_000_000_000 = 1_500_000_000
        assert_eq!(rpc_tx.inner.effective_gas_price, Some(1_500_000_000));
    }
}
