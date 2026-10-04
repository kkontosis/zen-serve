//! The `zen-serve` command (operations.md).

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use zen_server::config::Config;
use zen_server::supervisor::{self, Supervisor};

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
    /// Run the server (and this node's FoundationDB processes, if it was set
    /// up with `init` or `join`).
    Serve {
        /// Path to zen-serve.toml.
        #[arg(long, short)]
        config: PathBuf,
    },
    /// Create a FoundationDB cluster on this node, then serve.
    Init {
        /// Path to zen-serve.toml.
        #[arg(long, short)]
        config: PathBuf,
        /// Only run the FoundationDB processes, not the API.
        #[arg(long)]
        no_api: bool,
    },
    /// Add this node to a cluster, then serve. The token comes from
    /// `zen-serve token` on any node.
    Join {
        /// Join token.
        token: String,
        /// Path to zen-serve.toml.
        #[arg(long, short)]
        config: PathBuf,
        /// Only run the FoundationDB processes, not the API.
        #[arg(long)]
        no_api: bool,
    },
    /// Print the join token of this node's cluster.
    Token {
        /// Path to zen-serve.toml.
        #[arg(long, short)]
        config: PathBuf,
    },
    /// Print storage health.
    Status {
        /// Path to zen-serve.toml.
        #[arg(long, short)]
        config: PathBuf,
    },
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("zen-serve: {msg}");
    std::process::exit(1);
}

fn load(path: &PathBuf) -> Config {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    Config::from_toml(&text).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())))
}

fn supervise(cfg: &Config) -> Supervisor {
    Supervisor::start(cfg).unwrap_or_else(|e| fail(e))
}

/// Serve the API (if `api`) until Ctrl-C, then stop the supervisor.
async fn run(cfg: Config, sup: Option<Supervisor>, api: bool) {
    let server = if api {
        match zen_server::start(cfg).await {
            Ok(s) => Some(s),
            Err(e) => {
                if let Some(s) = sup {
                    s.shutdown().await;
                }
                fail(e);
            }
        }
    } else {
        None
    };
    let _ = tokio::signal::ctrl_c().await;
    if let Some(s) = server {
        s.abort();
    }
    if let Some(s) = sup {
        s.shutdown().await;
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zen_server=info".into()),
        )
        .init();
    match Cli::parse().cmd {
        Cmd::Serve { config } => {
            let cfg = load(&config);
            let sup = supervisor::managed(&cfg).then(|| supervise(&cfg));
            run(cfg, sup, true).await;
        }
        Cmd::Init { config, no_api } => {
            let cfg = load(&config);
            supervisor::write_cluster_file(&cfg, &supervisor::new_cluster_file(&cfg))
                .unwrap_or_else(|e| fail(e));
            let sup = supervise(&cfg);
            if let Err(e) = supervisor::configure_new(&cfg).await {
                sup.shutdown().await;
                fail(e);
            }
            print_token(&cfg);
            run(cfg, Some(sup), !no_api).await;
        }
        Cmd::Join {
            token,
            config,
            no_api,
        } => {
            let cfg = load(&config);
            let contents = supervisor::parse_join_token(&token).unwrap_or_else(|e| fail(e));
            supervisor::write_cluster_file(&cfg, &contents).unwrap_or_else(|e| fail(e));
            let sup = supervise(&cfg);
            run(cfg, Some(sup), !no_api).await;
        }
        Cmd::Token { config } => print_token(&load(&config)),
        Cmd::Status { config } => {
            let cfg = load(&config);
            let s = zen_server::cluster_status(&cfg).await;
            println!("backend:      {}", s.backend);
            println!("available:    {}", s.available);
            println!("healthy:      {}", s.healthy);
            if let Some(r) = &s.redundancy {
                println!("redundancy:   {r}");
            }
            println!("machines:     {}", s.machines);
            println!("processes:    {}", s.processes);
            println!("coordinators: {}", s.coordinators);
            for m in &s.messages {
                println!("message:      {m}");
            }
            if !s.available {
                std::process::exit(1);
            }
        }
    }
}

fn print_token(cfg: &Config) {
    let path = supervisor::cluster_file(cfg);
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| fail(format!("{}: {e} (not an init/join node)", path.display())));
    println!("join token: {}", supervisor::join_token(&contents));
}
