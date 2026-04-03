use alloy_primitives::U256;
use clap::Args;
use eyre::ensure;
use serde_json::json;

#[derive(Args)]
pub struct WriteGenesisArgs {
    /// Output path for the genesis JSON file.
    #[arg(long)]
    pub output: String,
    /// Hex-encoded sender address (0x-prefixed).
    #[arg(long)]
    pub sender: String,
    /// Sender balance in wei (decimal string).
    #[arg(long, default_value = "1000000000000000000000000000")]
    pub sender_balance: String,
}

/// Build a genesis JSON value for the benchmark chain.
///
/// All Morph hardforks are activated at block 0 / timestamp 0.
/// Uses MPT mode (`useZktrie: false`) since Jade is active at genesis.
pub fn build_genesis(sender: &str, sender_balance: &str) -> eyre::Result<serde_json::Value> {
    // Normalize sender: strip 0x prefix, lowercase.
    let sender_normalized = sender.strip_prefix("0x").unwrap_or(sender).to_ascii_lowercase();

    // Validate: must be exactly 40 hex characters.
    ensure!(
        sender_normalized.len() == 40 && sender_normalized.chars().all(|c| c.is_ascii_hexdigit()),
        "sender must be a 40-character hex address, got: {sender}"
    );

    // Parse balance as decimal U256, then format as 0x-prefixed hex.
    let balance = U256::from_str_radix(sender_balance, 10)
        .map_err(|e| eyre::eyre!("invalid sender_balance decimal: {e}"))?;
    let hex_balance = format!("{balance:#x}");

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
            "terminalTotalDifficulty": 0,
            "morph": {
                "useZktrie": false,
                "maxTxPerBlock": 10000,
                "maxTxPayloadBytesPerBlock": 131072000,
                "feeVaultAddress": "0x530000000000000000000000000000000000000a"
            }
        },
        "nonce": "0x0",
        "timestamp": "0x0",
        "extraData": "0x",
        "gasLimit": "0x30000000",
        "difficulty": "0x0",
        "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "coinbase": "0x0000000000000000000000000000000000000000",
        "alloc": {
            (sender_normalized): {
                "balance": hex_balance
            }
        }
    });

    Ok(genesis)
}

pub fn run(args: WriteGenesisArgs) -> eyre::Result<()> {
    let genesis = build_genesis(&args.sender, &args.sender_balance)?;
    let json_str = serde_json::to_string_pretty(&genesis)?;
    std::fs::write(&args.output, json_str)?;
    println!("Genesis written to {}", args.output);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_contains_sender_alloc_and_morph_config() {
        let sender = "0xABcdEF0123456789abcDEF0123456789aBCDeF01";
        let balance = "1000000000000000000000000000";
        let genesis = build_genesis(sender, balance).expect("should build genesis");

        // Sender appears in alloc (lowercase, no 0x prefix).
        let sender_key = "abcdef0123456789abcdef0123456789abcdef01";
        let alloc = genesis.get("alloc").expect("alloc must exist");
        let entry = alloc.get(sender_key).expect("sender must be in alloc");
        assert!(entry.get("balance").is_some(), "balance must be set");

        // Chain config must be present with all hardforks at 0.
        let config = genesis.get("config").expect("config must exist");
        assert_eq!(config["chainId"], 99999);
        assert_eq!(config["homesteadBlock"], 0);
        assert_eq!(config["jadeTime"], 0);
        assert_eq!(config["emeraldTime"], 0);
        assert_eq!(config["viridianTime"], 0);

        // Morph section.
        let morph = config.get("morph").expect("morph section must exist");
        assert_eq!(morph["useZktrie"], false);
        assert_eq!(morph["maxTxPerBlock"], 10000);
        assert_eq!(
            morph["feeVaultAddress"],
            "0x530000000000000000000000000000000000000a"
        );

        // Gas limit must be >= 800_000_000 (0x30000000 = 805306368).
        let gas_limit_str = genesis["gasLimit"].as_str().expect("gasLimit must be a string");
        let gas_limit =
            u64::from_str_radix(gas_limit_str.strip_prefix("0x").unwrap(), 16).unwrap();
        assert!(
            gas_limit >= 800_000_000,
            "gasLimit must be >= 800M, got {gas_limit}"
        );
    }

    #[test]
    fn genesis_rejects_invalid_sender() {
        // Too short — only 38 hex chars.
        let result = build_genesis("0xabcdef01234567890123456789012345678901", "1000");
        assert!(result.is_err(), "should reject short address");
    }
}
