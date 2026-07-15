use alloy_consensus::{EthereumTxEnvelope, SignableTransaction, TxEip1559, TxEip4844};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use rayon::prelude::*;

/// Concrete envelope type (EIP-4844 variant required by the generic).
type TxEnvelope = EthereumTxEnvelope<TxEip4844>;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Pre-determined address for the BenchToken ERC20 contract.
pub const BENCH_TOKEN_ADDR: Address = Address::new([
    0x55, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01,
]);

/// Pre-determined address for the BenchSwap contract.
pub const BENCH_SWAP_ADDR: Address = Address::new([
    0x55, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x02,
]);

/// Max fee per gas used for all benchmark transactions (1 gwei).
/// Must exceed the benchmark chain's baseFeePerGas.
pub const BENCH_MAX_FEE_PER_GAS: u128 = 1_000_000_000;
const PARALLEL_SIGN_THRESHOLD: usize = 64;

// ---------------------------------------------------------------------------
// Workload enum
// ---------------------------------------------------------------------------

/// The type of transaction workload to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    EthTransfer,
    Erc20Transfer,
    UniswapSwap,
}

impl Workload {
    /// Human-readable name for the workload (used in CLI args and reporting).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EthTransfer => "eth-transfer",
            Self::Erc20Transfer => "erc20-transfer",
            Self::UniswapSwap => "uniswap-swap",
        }
    }

    /// Approximate gas consumption per transaction for this workload type.
    pub fn gas_per_tx(&self) -> u64 {
        match self {
            Self::EthTransfer => 21_000,
            Self::Erc20Transfer => 34_300,
            Self::UniswapSwap => 150_000,
        }
    }
}

impl std::str::FromStr for Workload {
    type Err = eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "eth-transfer" => Ok(Self::EthTransfer),
            "erc20-transfer" => Ok(Self::Erc20Transfer),
            "uniswap-swap" => Ok(Self::UniswapSwap),
            other => Err(eyre::eyre!(
                "unknown workload: {other:?} (expected eth-transfer, erc20-transfer, or uniswap-swap)"
            )),
        }
    }
}

impl std::fmt::Display for Workload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// BenchSender
// ---------------------------------------------------------------------------

/// A benchmark sender: private key signer, its derived address, and a nonce counter.
pub struct BenchSender {
    pub signer: PrivateKeySigner,
    pub address: Address,
    pub nonce: u64,
}

/// Generate `count` deterministic benchmark senders.
///
/// Key derivation:
///   master = keccak256("bench-sender-0")
///   key[i] = keccak256(master ++ i.to_be_bytes())
///
/// Each sender starts at nonce 0.
pub fn generate_senders(count: usize) -> Vec<BenchSender> {
    let master = keccak256(b"bench-sender-0");
    (0..count)
        .map(|i| {
            let mut preimage = [0u8; 32 + 8];
            preimage[..32].copy_from_slice(master.as_slice());
            preimage[32..].copy_from_slice(&(i as u64).to_be_bytes());
            let secret = keccak256(preimage);
            let signer = PrivateKeySigner::from_bytes(&secret.into())
                .expect("deterministic key derivation should never fail");
            let address = signer.address();
            BenchSender {
                signer,
                address,
                nonce: 0,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Address / storage helpers
// ---------------------------------------------------------------------------

/// Generate a deterministic receiver address from an index.
///
/// Uses `0xBB` as the first byte and the index in the last 8 bytes.
pub fn receiver_address(index: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[0] = 0xBB;
    bytes[12..20].copy_from_slice(&index.to_be_bytes());
    Address::from(bytes)
}

/// Compute the storage slot for a Solidity `mapping(address => ...)` entry.
///
/// Equivalent to `keccak256(abi.encode(key, slot))`.
pub fn mapping_slot(key: Address, slot: U256) -> U256 {
    let mut preimage = [0u8; 64];
    // abi.encode(address, uint256): address is left-padded to 32 bytes
    preimage[12..32].copy_from_slice(key.as_slice());
    preimage[32..64].copy_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(preimage).0)
}

// ---------------------------------------------------------------------------
// Signing helper
// ---------------------------------------------------------------------------

/// Sign a `TxEip1559` and return the RLP-encoded envelope bytes.
fn sign_and_encode(
    signer: &PrivateKeySigner,
    tx: TxEip1559,
    _chain_id: u64,
) -> eyre::Result<Bytes> {
    let sig = signer
        .sign_hash_sync(&tx.signature_hash())
        .map_err(|e| eyre::eyre!("signing failed: {e}"))?;
    let envelope: TxEnvelope = EthereumTxEnvelope::Eip1559(tx.into_signed(sig));
    Ok(Bytes::from(envelope.encoded_2718()))
}

fn sign_batch(
    signer: &PrivateKeySigner,
    txs: Vec<TxEip1559>,
    chain_id: u64,
) -> eyre::Result<Vec<Bytes>> {
    if txs.len() < PARALLEL_SIGN_THRESHOLD {
        return txs
            .into_iter()
            .map(|tx| sign_and_encode(signer, tx, chain_id))
            .collect();
    }

    txs.into_par_iter()
        .map_init(
            || signer.clone(),
            move |signer, tx| sign_and_encode(signer, tx, chain_id),
        )
        .collect()
}

// ---------------------------------------------------------------------------
// Transaction builders
// ---------------------------------------------------------------------------

/// Build `count` signed ETH transfer transactions from `sender`.
///
/// Each transfer sends 1 wei to a deterministic receiver, with gas_limit=21000.
/// The sender's nonce is advanced by `count`.
pub fn build_eth_transfers(
    sender: &mut BenchSender,
    count: u64,
    chain_id: u64,
) -> eyre::Result<Vec<Bytes>> {
    let start_nonce = sender.nonce;
    let txs = (0..count)
        .map(|i| TxEip1559 {
            chain_id,
            nonce: start_nonce + i,
            gas_limit: 21_000,
            max_fee_per_gas: BENCH_MAX_FEE_PER_GAS,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(receiver_address(i)),
            value: U256::from(1),
            access_list: Default::default(),
            input: Bytes::new(),
        })
        .collect();
    sender.nonce += count;
    sign_batch(&sender.signer, txs, chain_id)
}

/// Build `count` signed ERC20 `transfer(address,uint256)` transactions from `sender`.
///
/// Each call transfers 1 token to a deterministic receiver via `BENCH_TOKEN_ADDR`,
/// with gas_limit=60000. The sender's nonce is advanced by `count`.
pub fn build_erc20_transfers(
    sender: &mut BenchSender,
    count: u64,
    chain_id: u64,
) -> eyre::Result<Vec<Bytes>> {
    let start_nonce = sender.nonce;
    let txs = (0..count)
        .map(|i| {
            let to = receiver_address(i);

            // transfer(address,uint256) selector: 0xa9059cbb
            let mut calldata = vec![0xa9, 0x05, 0x9c, 0xbb];
            calldata.extend_from_slice(&[0u8; 12]); // left-pad address to 32 bytes
            calldata.extend_from_slice(to.as_slice());
            calldata.extend_from_slice(&U256::from(1).to_be_bytes::<32>());

            TxEip1559 {
                chain_id,
                nonce: start_nonce + i,
                gas_limit: 60_000,
                max_fee_per_gas: BENCH_MAX_FEE_PER_GAS,
                max_priority_fee_per_gas: 0,
                to: TxKind::Call(BENCH_TOKEN_ADDR),
                value: U256::ZERO,
                access_list: Default::default(),
                input: Bytes::from(calldata),
            }
        })
        .collect();
    sender.nonce += count;
    sign_batch(&sender.signer, txs, chain_id)
}

/// Build `count` signed `BenchSwap.swap0For1(uint256)` transactions from `sender`.
///
/// Each call invokes swap0For1(1) on `BENCH_SWAP_ADDR`, with gas_limit=150000.
/// The sender's nonce is advanced by `count`.
pub fn build_swap_txs(
    sender: &mut BenchSender,
    count: u64,
    chain_id: u64,
) -> eyre::Result<Vec<Bytes>> {
    let selector = &keccak256(b"swap0For1(uint256)")[..4];
    let start_nonce = sender.nonce;
    let txs = (0..count)
        .map(|_| {
            let mut calldata = Vec::with_capacity(4 + 32);
            calldata.extend_from_slice(selector);
            calldata.extend_from_slice(&U256::from(1).to_be_bytes::<32>());

            TxEip1559 {
                chain_id,
                nonce: start_nonce,
                gas_limit: 150_000,
                max_fee_per_gas: BENCH_MAX_FEE_PER_GAS,
                max_priority_fee_per_gas: 0,
                to: TxKind::Call(BENCH_SWAP_ADDR),
                value: U256::ZERO,
                access_list: Default::default(),
                input: Bytes::from(calldata),
            }
        })
        .enumerate()
        .map(|(offset, mut tx)| {
            tx.nonce += offset as u64;
            tx
        })
        .collect();
    sender.nonce += count;
    sign_batch(&sender.signer, txs, chain_id)
}

/// Build a full block's worth of transactions, distributed round-robin across senders.
///
/// Returns an interleaved `Vec<Bytes>` with exactly `total_txs` encoded transactions.
pub fn build_block_txs(
    senders: &mut [BenchSender],
    workload: Workload,
    total_txs: u64,
    chain_id: u64,
) -> eyre::Result<Vec<Bytes>> {
    assert!(!senders.is_empty(), "must have at least one sender");

    let num_senders = senders.len() as u64;
    // Compute how many txs each sender gets (round-robin distribution).
    let base_per_sender = total_txs / num_senders;
    let remainder = total_txs % num_senders;

    // Build + sign per-sender batches in parallel across all cores.
    let per_sender_txs: Vec<Vec<Bytes>> = senders
        .par_iter_mut()
        .enumerate()
        .map(|(idx, sender)| {
            let count = base_per_sender + if (idx as u64) < remainder { 1 } else { 0 };
            if count == 0 {
                return Ok(Vec::new());
            }
            match workload {
                Workload::EthTransfer => build_eth_transfers(sender, count, chain_id),
                Workload::Erc20Transfer => build_erc20_transfers(sender, count, chain_id),
                Workload::UniswapSwap => build_swap_txs(sender, count, chain_id),
            }
        })
        .collect::<eyre::Result<Vec<Vec<Bytes>>>>()?;

    // Interleave round-robin: take one tx from each sender in turn.
    // Convert each inner Vec into a drain iterator to avoid cloning Bytes.
    let mut iters: Vec<std::vec::IntoIter<Bytes>> =
        per_sender_txs.into_iter().map(|v| v.into_iter()).collect();
    let mut result = Vec::with_capacity(total_txs as usize);
    loop {
        let mut any = false;
        for iter in &mut iters {
            if let Some(tx) = iter.next() {
                result.push(tx);
                any = true;
            }
        }
        if !any {
            break;
        }
    }

    debug_assert_eq!(result.len(), total_txs as usize);
    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Transaction as _;
    use alloy_eips::eip2718::Decodable2718;
    use std::collections::HashSet;

    fn decode_nonce(tx: &Bytes) -> u64 {
        TxEnvelope::decode_2718_exact(tx.as_ref()).unwrap().nonce()
    }

    #[test]
    fn generate_senders_produces_unique_addresses() {
        let senders = generate_senders(100);
        assert_eq!(senders.len(), 100);
        let addrs: HashSet<Address> = senders.iter().map(|s| s.address).collect();
        assert_eq!(addrs.len(), 100, "all sender addresses must be unique");
    }

    #[test]
    fn generate_senders_is_deterministic() {
        let a = generate_senders(10);
        let b = generate_senders(10);
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.address, y.address);
        }
    }

    #[test]
    fn receiver_addresses_are_unique() {
        let addrs: HashSet<Address> = (0..100).map(receiver_address).collect();
        assert_eq!(addrs.len(), 100);
    }

    #[test]
    fn mapping_slot_is_deterministic() {
        let addr = Address::with_last_byte(0x42);
        let slot = U256::from(3);
        let a = mapping_slot(addr, slot);
        let b = mapping_slot(addr, slot);
        assert_eq!(a, b);
        // Different inputs produce different slots.
        let c = mapping_slot(Address::with_last_byte(0x43), slot);
        assert_ne!(a, c);
    }

    #[test]
    fn workload_roundtrip() {
        for w in [
            Workload::EthTransfer,
            Workload::Erc20Transfer,
            Workload::UniswapSwap,
        ] {
            let parsed: Workload = w.as_str().parse().unwrap();
            assert_eq!(parsed, w);
        }
    }

    #[test]
    fn workload_from_str_rejects_unknown() {
        assert!("unknown".parse::<Workload>().is_err());
    }

    #[test]
    fn build_eth_transfers_advances_nonce() {
        let mut senders = generate_senders(1);
        let txs = build_eth_transfers(&mut senders[0], 5, 99999).unwrap();
        assert_eq!(txs.len(), 5);
        assert_eq!(senders[0].nonce, 5);
        assert_eq!(decode_nonce(&txs[0]), 0);
        assert_eq!(decode_nonce(&txs[4]), 4);
    }

    #[test]
    fn build_erc20_transfers_advances_nonce() {
        let mut senders = generate_senders(1);
        let txs = build_erc20_transfers(&mut senders[0], 3, 99999).unwrap();
        assert_eq!(txs.len(), 3);
        assert_eq!(senders[0].nonce, 3);
        assert_eq!(decode_nonce(&txs[0]), 0);
        assert_eq!(decode_nonce(&txs[2]), 2);
    }

    #[test]
    fn build_swap_txs_advances_nonce() {
        let mut senders = generate_senders(1);
        let txs = build_swap_txs(&mut senders[0], 4, 99999).unwrap();
        assert_eq!(txs.len(), 4);
        assert_eq!(senders[0].nonce, 4);
        assert_eq!(decode_nonce(&txs[0]), 0);
        assert_eq!(decode_nonce(&txs[3]), 3);
    }

    #[test]
    fn build_block_txs_distributes_round_robin() {
        let mut senders = generate_senders(3);
        let txs = build_block_txs(&mut senders, Workload::EthTransfer, 10, 99999).unwrap();
        assert_eq!(txs.len(), 10);
        // First two senders get 4 txs each (10/3=3 remainder 1 => first 1 gets +1... wait)
        // 10 / 3 = 3 base, remainder = 1, so sender 0 gets 4, senders 1,2 get 3 each.
        assert_eq!(senders[0].nonce, 4);
        assert_eq!(senders[1].nonce, 3);
        assert_eq!(senders[2].nonce, 3);
    }

    #[test]
    fn build_block_txs_handles_fewer_txs_than_senders() {
        let mut senders = generate_senders(5);
        let txs = build_block_txs(&mut senders, Workload::EthTransfer, 3, 99999).unwrap();
        assert_eq!(txs.len(), 3);
        // Only first 3 senders should have produced 1 tx each.
        assert_eq!(senders[0].nonce, 1);
        assert_eq!(senders[1].nonce, 1);
        assert_eq!(senders[2].nonce, 1);
        assert_eq!(senders[3].nonce, 0);
        assert_eq!(senders[4].nonce, 0);
    }

    #[test]
    fn bench_constants_are_correct() {
        assert_eq!(
            BENCH_TOKEN_ADDR,
            "0x5500000000000000000000000000000000000001"
                .parse::<Address>()
                .unwrap()
        );
        assert_eq!(
            BENCH_SWAP_ADDR,
            "0x5500000000000000000000000000000000000002"
                .parse::<Address>()
                .unwrap()
        );
        assert_eq!(BENCH_MAX_FEE_PER_GAS, 1_000_000_000);
    }
}
