//! `zen-serve serve --config zen-serve.toml`

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use zen_server::config::Config;

#[derive(Parser)]
#[command(
    name = "zen-serve",
    version,
    about = "Keyless end-to-end encrypted storage server"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server.
    Serve {
        /// Path to zen-serve.toml.
        #[arg(long, short)]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zen_server=info".into()),
        )
        .init();
    let Cmd::Serve { config } = Cli::parse().cmd;
    let text = match std::fs::read_to_string(&config) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("zen-serve: {}: {e}", config.display());
            std::process::exit(2);
        }
    };
    let cfg = match Config::from_toml(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zen-serve: {}: {e}", config.display());
            std::process::exit(2);
        }
    };
    let server = match zen_server::start(cfg).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("zen-serve: {e}");
            std::process::exit(1);
        }
    };
    let _ = tokio::signal::ctrl_c().await;
    server.abort();
}
