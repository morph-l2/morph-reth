mod engine;
mod genesis;
pub mod mode_e2e;
pub mod mode_exec;
pub mod mode_openloop;
pub mod mode_sustained;
mod report;
pub mod sweep;
pub mod tx_factory;
mod verify;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bench-block-exec", about = "Morph block execution benchmark")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a benchmark genesis file.
    WriteGenesis(genesis::WriteGenesisArgs),
    /// Run a benchmark in the specified mode.
    Run {
        #[command(subcommand)]
        mode: RunMode,
    },
    /// Automatically find the TPS inflection point.
    Sweep(sweep::SweepArgs),
    /// Verify state consistency between two nodes.
    VerifyState(verify::VerifyStateArgs),
    /// Summarize benchmark results into TSV.
    Summarize(report::SummarizeArgs),
}

#[derive(Subcommand)]
enum RunMode {
    /// Mode A: Block execution after untimed txpool preload.
    Exec(mode_exec::ExecArgs),
    /// Mode B: End-to-end (txpool -> assembly -> import).
    E2e(mode_e2e::E2eArgs),
    /// Mode C: Sustained block production with optional warmup.
    Sustained(mode_sustained::SustainedArgs),
    /// Tempo-style open-loop load with continuous submit and background import.
    Openloop(mode_openloop::OpenLoopArgs),
    /// Produce Morph blocks for an external transaction generator.
    Produce(mode_openloop::ExternalProducerArgs),
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::WriteGenesis(args) => genesis::run(args),
        Command::Run { mode } => match mode {
            RunMode::Exec(args) => mode_exec::run(args).await,
            RunMode::E2e(args) => mode_e2e::run(args).await,
            RunMode::Sustained(args) => mode_sustained::run(args).await,
            RunMode::Openloop(args) => mode_openloop::run(args).await,
            RunMode::Produce(args) => mode_openloop::run_external_producer(args).await,
        },
        Command::Sweep(args) => sweep::run(args).await,
        Command::VerifyState(args) => verify::run(args).await,
        Command::Summarize(args) => report::summarize(args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn run_help_lists_openloop_mode() {
        let mut command = Cli::command();
        let run = command
            .find_subcommand_mut("run")
            .expect("run subcommand should exist");
        let help = run.render_long_help().to_string();

        assert!(
            help.contains("openloop"),
            "run help should list the openloop mode, got: {help}"
        );
    }

    #[test]
    fn cli_parses_openloop_mode() {
        let parsed = Cli::try_parse_from([
            "bench-block-exec",
            "run",
            "openloop",
            "--engine-rpc",
            "http://127.0.0.1:8551",
            "--jwt-secret",
            "./local-test/jwt-secret.txt",
            "--http-rpc",
            "http://127.0.0.1:8545",
            "--workload",
            "eth-transfer",
            "--target-tps",
            "20000",
            "--duration-secs",
            "30",
            "--senders",
            "100",
            "--output",
            "bench-results/openloop.jsonl",
            "--chain-id",
            "99999",
        ]);

        assert!(parsed.is_ok(), "expected openloop mode to parse");
    }

    #[test]
    fn cli_parses_legacy_small_set_for_exec() {
        let parsed = Cli::try_parse_from([
            "bench-block-exec",
            "run",
            "exec",
            "--jwt-secret",
            "./local-test/jwt-secret.txt",
            "--workload",
            "eth-transfer",
            "--txs-per-block",
            "1000",
            "--blocks",
            "1",
            "--output",
            "/tmp/exec.jsonl",
            "--receiver-mode",
            "legacy-small-set",
        ])
        .unwrap();

        let Command::Run {
            mode: RunMode::Exec(args),
        } = parsed.command
        else {
            panic!("expected exec mode");
        };
        assert_eq!(args.receiver_mode, tx_factory::ReceiverMode::LegacySmallSet);
    }

    #[test]
    fn cli_parses_legacy_small_set_for_sustained() {
        let parsed = Cli::try_parse_from([
            "bench-block-exec",
            "run",
            "sustained",
            "--jwt-secret",
            "./local-test/jwt-secret.txt",
            "--workload",
            "erc20-transfer",
            "--txs-per-block",
            "50000",
            "--blocks",
            "1",
            "--output",
            "/tmp/sustained.jsonl",
            "--receiver-mode",
            "legacy-small-set",
        ])
        .unwrap();

        let Command::Run {
            mode: RunMode::Sustained(args),
        } = parsed.command
        else {
            panic!("expected sustained mode");
        };
        assert_eq!(args.receiver_mode, tx_factory::ReceiverMode::LegacySmallSet);
    }

    #[test]
    fn cli_parses_legacy_small_set_for_openloop() {
        let parsed = Cli::try_parse_from([
            "bench-block-exec",
            "run",
            "openloop",
            "--jwt-secret",
            "./local-test/jwt-secret.txt",
            "--workload",
            "eth-transfer",
            "--target-tps",
            "200000",
            "--duration-secs",
            "1",
            "--output",
            "/tmp/openloop.jsonl",
            "--receiver-mode",
            "legacy-small-set",
        ])
        .unwrap();

        let Command::Run {
            mode: RunMode::Openloop(args),
        } = parsed.command
        else {
            panic!("expected openloop mode");
        };
        assert_eq!(args.receiver_mode, tx_factory::ReceiverMode::LegacySmallSet);
    }

    #[test]
    fn cli_parses_external_producer_mode() {
        let parsed = Cli::try_parse_from([
            "bench-block-exec",
            "run",
            "produce",
            "--jwt-secret",
            "./local-test/jwt-secret.txt",
            "--output",
            "/tmp/producer.jsonl",
            "--stop-file",
            "/tmp/producer.stop",
        ]);

        assert!(parsed.is_ok(), "expected external producer mode to parse");
    }
}
