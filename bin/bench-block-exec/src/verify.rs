use clap::Args;
use serde::{Deserialize, Serialize};

use crate::tx_factory::generate_senders;

#[derive(Args)]
pub(crate) struct VerifyStateArgs {
    /// RPC URL for node A.
    #[arg(long)]
    pub rpc_a: String,
    /// RPC URL for node B.
    #[arg(long)]
    pub rpc_b: String,
    /// Number of accounts to sample for balance checks.
    #[arg(long, default_value = "100")]
    pub check_balances: u64,
    /// Output file path for verification results.
    #[arg(long)]
    pub output: Option<String>,
}

#[derive(Debug, Serialize)]
struct VerifyResult {
    status: String,
    block_number_a: String,
    block_number_b: String,
    state_root_match: bool,
    receipts_root_match: bool,
    balances_checked: u64,
    balances_matched: u64,
    mismatches: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LatestBlock {
    number: String,
    state_root: String,
    receipts_root: String,
}

/// Send a JSON-RPC request and return the `result` field.
async fn rpc_call(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: serde_json::Value,
) -> eyre::Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": 1
    });

    let resp: serde_json::Value = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| eyre::eyre!("RPC request to {url} failed: {e}"))?
        .json()
        .await
        .map_err(|e| eyre::eyre!("failed to parse RPC response from {url}: {e}"))?;

    if let Some(err) = resp.get("error") {
        return Err(eyre::eyre!("RPC error from {url}: {err}"));
    }

    resp.get("result")
        .cloned()
        .ok_or_else(|| eyre::eyre!("RPC response from {url} missing 'result' field"))
}

async fn latest_block(client: &reqwest::Client, url: &str) -> eyre::Result<LatestBlock> {
    let value = rpc_call(
        client,
        url,
        "eth_getBlockByNumber",
        serde_json::json!(["latest", false]),
    )
    .await?;

    serde_json::from_value(value)
        .map_err(|e| eyre::eyre!("invalid latest block response from {url}: {e}"))
}

pub(crate) async fn run(args: VerifyStateArgs) -> eyre::Result<()> {
    let client = reqwest::Client::new();

    // 1. Fetch latest block from both nodes.
    let block_a = latest_block(&client, &args.rpc_a).await?;
    let block_b = latest_block(&client, &args.rpc_b).await?;

    let number_a = block_a.number;
    let number_b = block_b.number;

    let mut mismatches: Vec<String> = Vec::new();

    // 2. Compare block numbers.
    if number_a != number_b {
        mismatches.push(format!("block number mismatch: A={number_a}, B={number_b}"));
    }

    // 3. Compare state roots.
    let state_root_a = block_a.state_root;
    let state_root_b = block_b.state_root;
    let state_root_match = state_root_a == state_root_b;
    if !state_root_match {
        mismatches.push(format!(
            "stateRoot mismatch: A={state_root_a}, B={state_root_b}"
        ));
    }

    // 4. Compare receipts roots.
    let receipts_root_a = block_a.receipts_root;
    let receipts_root_b = block_b.receipts_root;
    let receipts_root_match = receipts_root_a == receipts_root_b;
    if !receipts_root_match {
        mismatches.push(format!(
            "receiptsRoot mismatch: A={receipts_root_a}, B={receipts_root_b}"
        ));
    }

    // 5. Compare balances for deterministic funded sender accounts.
    let mut balances_matched: u64 = 0;
    for sender in generate_senders(args.check_balances as usize) {
        let addr = sender.address;
        let addr_hex = format!("{addr:#x}");

        let balance_a = rpc_call(
            &client,
            &args.rpc_a,
            "eth_getBalance",
            serde_json::json!([addr_hex, "latest"]),
        )
        .await?;

        let balance_b = rpc_call(
            &client,
            &args.rpc_b,
            "eth_getBalance",
            serde_json::json!([addr_hex, "latest"]),
        )
        .await?;

        if balance_a == balance_b {
            balances_matched += 1;
        } else {
            mismatches.push(format!(
                "balance mismatch for {addr_hex}: A={balance_a}, B={balance_b}"
            ));
        }
    }

    // 6. Build result.
    let status = if mismatches.is_empty() {
        "PASS".to_string()
    } else {
        "FAIL".to_string()
    };

    let result = VerifyResult {
        status: status.clone(),
        block_number_a: number_a,
        block_number_b: number_b,
        state_root_match,
        receipts_root_match,
        balances_checked: args.check_balances,
        balances_matched,
        mismatches: mismatches.clone(),
    };

    // 7. Write to output file if specified.
    if let Some(ref path) = args.output {
        let json = serde_json::to_string_pretty(&result)?;
        std::fs::write(path, json)
            .map_err(|e| eyre::eyre!("failed to write output to {path}: {e}"))?;
    }

    // 8. Print summary to stderr.
    eprintln!("--- Verify State ---");
    eprintln!("Status:             {status}");
    eprintln!("Block number A:     {}", result.block_number_a);
    eprintln!("Block number B:     {}", result.block_number_b);
    eprintln!("State root match:   {state_root_match}");
    eprintln!("Receipts root match:{receipts_root_match}");
    eprintln!(
        "Balances checked:   {}/{}",
        balances_matched, args.check_balances
    );
    if !mismatches.is_empty() {
        eprintln!("Mismatches:");
        for m in &mismatches {
            eprintln!("  - {m}");
        }
    }

    // 9. Fail if any mismatches found.
    if !mismatches.is_empty() {
        eyre::bail!(
            "state verification FAILED with {} mismatch(es)",
            mismatches.len()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::LatestBlock;

    #[test]
    fn latest_block_requires_comparison_roots() {
        let missing_receipts_root = serde_json::json!({
            "number": "0x1",
            "stateRoot": "0xstate"
        });

        assert!(serde_json::from_value::<LatestBlock>(missing_receipts_root).is_err());
    }

    #[test]
    fn latest_block_rejects_null_comparison_roots() {
        let null_state_root = serde_json::json!({
            "number": "0x1",
            "stateRoot": null,
            "receiptsRoot": "0xreceipts"
        });

        assert!(serde_json::from_value::<LatestBlock>(null_state_root).is_err());
    }
}
