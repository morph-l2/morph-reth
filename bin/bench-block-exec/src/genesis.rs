use alloy_primitives::U256;
use clap::Args;
use eyre::ensure;
use morph_chainspec::MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK;
use serde_json::{Map, json};

use crate::tx_factory;

#[derive(Args)]
pub(crate) struct WriteGenesisArgs {
    /// Output path for the genesis JSON file.
    #[arg(long)]
    pub output: String,
    /// Legacy single-sender address (hex, 0x-prefixed). Optional if --senders is used.
    #[arg(long)]
    pub sender: Option<String>,
    /// Sender balance in wei (decimal string).
    #[arg(long, default_value = "1000000000000000000000000000")]
    pub sender_balance: String,
    /// Number of benchmark senders to generate (deterministic keys).
    #[arg(long, default_value = "0")]
    pub senders: u64,
    /// Gas limit for the genesis block (hex string, e.g. 0x2540BE400).
    #[arg(long, default_value = "0x2540BE400")]
    pub gas_limit: String,
    /// Max transactions per block. 0 = use 10,000,000 (effectively unlimited).
    #[arg(long, default_value = "10000")]
    pub max_tx_per_block: u64,
    /// Pre-deploy BenchToken bytecode (hex). If set, deploys at BENCH_TOKEN_ADDR.
    #[arg(long)]
    pub bench_token_code: Option<String>,
    /// Pre-deploy BenchSwap bytecode (hex). If set, deploys at BENCH_SWAP_ADDR.
    #[arg(long)]
    pub bench_swap_code: Option<String>,
}

/// Fee vault address used in Morph config and pre-funded in genesis.
const FEE_VAULT_ADDR: &str = "530000000000000000000000000000000000000a";

/// Build a genesis JSON value for the benchmark chain.
///
/// All Morph hardforks are activated at block 0 / timestamp 0.
/// Uses MPT mode (`useZktrie: false`) since Jade is active at genesis.
pub(crate) fn build_genesis(args: &WriteGenesisArgs) -> eyre::Result<serde_json::Value> {
    let hex_balance = {
        let balance = U256::from_str_radix(&args.sender_balance, 10)
            .map_err(|e| eyre::eyre!("invalid sender_balance decimal: {e}"))?;
        format!("{balance:#x}")
    };

    // Resolve max_tx_per_block: 0 means effectively unlimited (10M).
    let max_tx = if args.max_tx_per_block == 0 {
        10_000_000u64
    } else {
        args.max_tx_per_block
    };

    // Build alloc map.
    let mut alloc = Map::new();

    // Always include the fee vault with zero balance so the account exists.
    alloc.insert(FEE_VAULT_ADDR.to_string(), json!({ "balance": "0x0" }));

    // Collect all sender addresses for contract storage population.
    let mut sender_addresses = Vec::new();

    // Legacy single sender (--sender).
    if let Some(ref sender) = args.sender {
        let normalized = sender
            .strip_prefix("0x")
            .unwrap_or(sender)
            .to_ascii_lowercase();
        ensure!(
            normalized.len() == 40 && normalized.chars().all(|c| c.is_ascii_hexdigit()),
            "sender must be a 40-character hex address, got: {sender}"
        );
        alloc.insert(normalized, json!({ "balance": &hex_balance }));
        // Parse as Address for contract storage.
        let addr: alloy_primitives::Address = sender
            .parse()
            .map_err(|e| eyre::eyre!("failed to parse sender address: {e}"))?;
        sender_addresses.push(addr);
    }

    // Multi-sender (--senders N).
    if args.senders > 0 {
        let bench_senders = tx_factory::generate_senders(args.senders as usize);
        for s in &bench_senders {
            let addr_hex = format!("{:x}", s.address);
            alloc.insert(addr_hex, json!({ "balance": &hex_balance }));
            sender_addresses.push(s.address);
        }
    }

    // Pre-deploy BenchToken at BENCH_TOKEN_ADDR.
    if let Some(ref token_code) = args.bench_token_code {
        let code = normalize_hex_code(token_code);
        let mut storage = Map::new();

        // Slot 0: totalSupply = 10^30
        let total_supply = U256::from(10u64).pow(U256::from(30));
        storage.insert(
            format!("{:#066x}", U256::ZERO),
            json!(format!("{total_supply:#066x}")),
        );

        // For each sender: balanceOf[sender] at mapping slot 1 = 10^27
        let per_sender_balance = U256::from(10u64).pow(U256::from(27));
        for addr in &sender_addresses {
            let slot_key = tx_factory::mapping_slot(*addr, U256::from(1));
            storage.insert(
                format!("{slot_key:#066x}"),
                json!(format!("{per_sender_balance:#066x}")),
            );
        }

        let addr_hex = format!("{:x}", tx_factory::BENCH_TOKEN_ADDR);
        alloc.insert(
            addr_hex,
            json!({
                "balance": "0x0",
                "code": code,
                "storage": storage,
            }),
        );
    }

    // Pre-deploy BenchSwap at BENCH_SWAP_ADDR.
    if let Some(ref swap_code) = args.bench_swap_code {
        let code = normalize_hex_code(swap_code);
        let mut storage = Map::new();

        let reserve = U256::from(10u64).pow(U256::from(24));

        // Slot 0: reserve0 = 10^24
        storage.insert(
            format!("{:#066x}", U256::ZERO),
            json!(format!("{reserve:#066x}")),
        );

        // Slot 1: reserve1 = 10^24
        storage.insert(
            format!("{:#066x}", U256::from(1)),
            json!(format!("{reserve:#066x}")),
        );

        // For each sender: balance0[sender] at mapping slot 2 = 10^24
        for addr in &sender_addresses {
            let slot_key = tx_factory::mapping_slot(*addr, U256::from(2));
            storage.insert(
                format!("{slot_key:#066x}"),
                json!(format!("{reserve:#066x}")),
            );
        }

        let addr_hex = format!("{:x}", tx_factory::BENCH_SWAP_ADDR);
        alloc.insert(
            addr_hex,
            json!({
                "balance": "0x0",
                "code": code,
                "storage": storage,
            }),
        );
    }

    let genesis = json!({
        "config": {
            "chainId": 99999,
            "homesteadBlock": 0,
            "eip150Block": 0,
            "eip150Hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "eip155Block": 0,
            "eip158Block": 0,
            "byzantiumBlock": 0,
            "constantinopleBlock": 0,
            "petersburgBlock": 0,
            "istanbulBlock": 0,
            "berlinBlock": 0,
            "londonBlock": 0,
            "archimedesBlock": 0,
            "shanghaiBlock": 0,
            "bernoulliBlock": 0,
            "curieBlock": 0,
            "morph203Time": 0,
            "viridianTime": 0,
            "emeraldTime": 0,
            "jadeTime": 0,
            "jadeForkTime": 0,
            "terminalTotalDifficulty": 0,
            "morph": {
                "useZktrie": false,
                "maxTxPerBlock": max_tx,
                "maxTxPayloadBytesPerBlock": MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK,
                "feeVaultAddress": format!("0x{FEE_VAULT_ADDR}")
            }
        },
        "nonce": "0x0",
        "timestamp": "0x0",
        "extraData": "0x",
        "gasLimit": &args.gas_limit,
        "difficulty": "0x0",
        "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "coinbase": "0x0000000000000000000000000000000000000000",
        "alloc": alloc
    });

    Ok(genesis)
}

/// Ensure hex code string has 0x prefix.
fn normalize_hex_code(code: &str) -> String {
    if code.starts_with("0x") || code.starts_with("0X") {
        code.to_string()
    } else {
        format!("0x{code}")
    }
}

pub(crate) fn run(args: WriteGenesisArgs) -> eyre::Result<()> {
    let genesis = build_genesis(&args)?;
    let json_str = serde_json::to_string_pretty(&genesis)?;
    std::fs::write(&args.output, json_str)?;
    println!("Genesis written to {}", args.output);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_args() -> WriteGenesisArgs {
        WriteGenesisArgs {
            output: "/dev/null".to_string(),
            sender: None,
            sender_balance: "1000000000000000000000000000".to_string(),
            senders: 0,
            gas_limit: "0x2540BE400".to_string(),
            max_tx_per_block: 10000,
            bench_token_code: None,
            bench_swap_code: None,
        }
    }

    #[test]
    fn genesis_with_legacy_sender() {
        let mut args = default_args();
        args.sender = Some("0xABcdEF0123456789abcDEF0123456789aBCDeF01".to_string());
        let genesis = build_genesis(&args).expect("should build genesis");

        let sender_key = "abcdef0123456789abcdef0123456789abcdef01";
        let alloc = genesis.get("alloc").expect("alloc must exist");
        let entry = alloc.get(sender_key).expect("sender must be in alloc");
        assert!(entry.get("balance").is_some(), "balance must be set");
    }

    #[test]
    fn genesis_with_multi_senders() {
        let mut args = default_args();
        args.senders = 3;
        let genesis = build_genesis(&args).expect("should build genesis");

        let alloc = genesis.get("alloc").expect("alloc must exist");
        let alloc_obj = alloc.as_object().expect("alloc is object");
        // 3 senders + 1 fee vault = 4 accounts.
        assert_eq!(
            alloc_obj.len(),
            4,
            "expected 4 alloc entries, got {}",
            alloc_obj.len()
        );
    }

    #[test]
    fn genesis_max_tx_per_block_zero_becomes_10m() {
        let mut args = default_args();
        args.max_tx_per_block = 0;
        let genesis = build_genesis(&args).expect("should build genesis");

        let morph = &genesis["config"]["morph"];
        assert_eq!(morph["maxTxPerBlock"], 10_000_000);
    }

    #[test]
    fn genesis_gas_limit_from_args() {
        let mut args = default_args();
        args.gas_limit = "0x1234".to_string();
        let genesis = build_genesis(&args).expect("should build genesis");
        assert_eq!(genesis["gasLimit"], "0x1234");
    }

    #[test]
    fn genesis_rejects_invalid_sender() {
        let mut args = default_args();
        // Too short -- only 38 hex chars.
        args.sender = Some("0xabcdef01234567890123456789012345678901".to_string());
        let result = build_genesis(&args);
        assert!(result.is_err(), "should reject short address");
    }

    #[test]
    fn genesis_contains_morph_config() {
        let args = default_args();
        let genesis = build_genesis(&args).expect("should build genesis");

        let config = genesis.get("config").expect("config must exist");
        assert_eq!(config["chainId"], 99999);
        assert_eq!(config["homesteadBlock"], 0);
        assert_eq!(config["jadeTime"], 0);
        assert_eq!(config["emeraldTime"], 0);
        assert_eq!(config["viridianTime"], 0);

        let morph = config.get("morph").expect("morph section must exist");
        assert_eq!(morph["useZktrie"], false);
        assert_eq!(
            morph["feeVaultAddress"],
            "0x530000000000000000000000000000000000000a"
        );
    }

    #[test]
    fn genesis_with_bench_token_predeploy() {
        let mut args = default_args();
        args.senders = 2;
        args.bench_token_code = Some("0x6001".to_string());
        let genesis = build_genesis(&args).expect("should build genesis");

        let alloc = genesis.get("alloc").expect("alloc must exist");
        let token_addr = format!("{:x}", tx_factory::BENCH_TOKEN_ADDR);
        let token_entry = alloc.get(&token_addr).expect("BenchToken must be in alloc");
        assert_eq!(token_entry["code"], "0x6001");
        assert!(token_entry.get("storage").is_some(), "storage must be set");
    }

    #[test]
    fn genesis_with_bench_swap_predeploy() {
        let mut args = default_args();
        args.senders = 2;
        args.bench_swap_code = Some("0x6002".to_string());
        let genesis = build_genesis(&args).expect("should build genesis");

        let alloc = genesis.get("alloc").expect("alloc must exist");
        let swap_addr = format!("{:x}", tx_factory::BENCH_SWAP_ADDR);
        let swap_entry = alloc.get(&swap_addr).expect("BenchSwap must be in alloc");
        assert_eq!(swap_entry["code"], "0x6002");

        let storage = swap_entry.get("storage").expect("storage must exist");
        // Should have slot 0, slot 1, and one mapping slot per sender.
        let storage_obj = storage.as_object().expect("storage is object");
        // 2 reserve slots + 2 sender balance slots = 4
        assert_eq!(storage_obj.len(), 4, "expected 4 storage slots");
    }
}
