//! The `zen-serve` command (operations.md).

use clap::{Parser, Subcommand};
use std::path::PathBuf;
#[cfg(feature = "fdb")]
use zen_server::config::Backend;
use zen_server::config::Config;
use zen_server::dump;
#[cfg(feature = "fdb")]
use zen_server::supervisor::{self, Supervisor};
use zen_store::embedded::{Embedded, Options};

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
    #[cfg(feature = "fdb")]
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
    #[cfg(feature = "fdb")]
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
    #[cfg(feature = "fdb")]
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
    /// FoundationDB continuous backup (needs `[backup] agents = true` on at
    /// least one node).
    #[cfg(feature = "fdb")]
    Backup {
        #[command(subcommand)]
        cmd: BackupCmd,
    },
    /// Restore a FoundationDB backup (point in time with --timestamp or
    /// --version). Without --add-prefix the cluster must be empty.
    #[cfg(feature = "fdb")]
    Restore {
        /// Path to zen-serve.toml.
        #[arg(long, short)]
        config: PathBuf,
        /// Backup URL (`file:///…` or `blobstore://…`).
        #[arg(long)]
        source: String,
        /// Restore to this time, `YYYY/MM/DD.HH:MI:SS[+/-]HHMM`.
        #[arg(long, conflicts_with = "version")]
        timestamp: Option<String>,
        /// Restore to this database version.
        #[arg(long)]
        version: Option<u64>,
        /// Cluster file of the database the backup was taken from; with
        /// --timestamp it translates the time into that database's version.
        /// Default: the target cluster (right when restoring into the same
        /// cluster, e.g. a clone with --add-prefix).
        #[arg(long)]
        orig_cluster_file: Option<String>,
        /// Restore under this key prefix: a clone served with
        /// `[storage] key_prefix`.
        #[arg(long)]
        add_prefix: Option<String>,
    },
    /// Write the keyspace (ciphertext only) to a file.
    Export {
        /// Path to zen-serve.toml.
        #[arg(long, short)]
        config: PathBuf,
        /// Output file.
        #[arg(long)]
        out: PathBuf,
    },
    /// Load an export into an empty store.
    Import {
        /// Path to zen-serve.toml.
        #[arg(long, short)]
        config: PathBuf,
        /// Export file.
        #[arg(long = "in")]
        input: PathBuf,
        /// Merge into a store that already holds data.
        #[arg(long)]
        force: bool,
    },
    /// Copy an embedded store into the configured backend (servers stopped).
    Migrate {
        /// Path to zen-serve.toml (the target).
        #[arg(long, short)]
        config: PathBuf,
        /// `data_dir` of the embedded server to copy from.
        #[arg(long)]
        from_data_dir: PathBuf,
        /// Merge into a store that already holds data.
        #[arg(long)]
        force: bool,
    },
}

#[cfg(feature = "fdb")]
#[derive(Subcommand)]
enum BackupCmd {
    /// Start a continuous backup.
    Start {
        /// Path to zen-serve.toml.
        #[arg(long, short)]
        config: PathBuf,
        /// Backup URL (`file:///…` or `blobstore://…`).
        #[arg(long)]
        dest: String,
        /// Seconds between snapshots (default: FoundationDB's, 10 days).
        #[arg(long)]
        snapshot_interval: Option<u64>,
    },
    /// Show backup progress.
    Status {
        /// Path to zen-serve.toml.
        #[arg(long, short)]
        config: PathBuf,
    },
    /// Stop the backup once it is restorable.
    Stop {
        /// Path to zen-serve.toml.
        #[arg(long, short)]
        config: PathBuf,
    },
    /// Show the restorable range of a backup.
    Describe {
        /// Path to zen-serve.toml.
        #[arg(long, short)]
        config: PathBuf,
        /// Backup URL.
        #[arg(long)]
        dest: String,
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

/// The FoundationDB process supervisor (nothing without the `fdb` feature).
#[cfg(not(feature = "fdb"))]
struct Supervisor;

#[cfg(not(feature = "fdb"))]
impl Supervisor {
    async fn shutdown(self) {}
}

#[cfg(feature = "fdb")]
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
    shutdown_signal().await;
    if let Some(s) = server {
        s.abort();
    }
    if let Some(s) = sup {
        s.shutdown().await;
    }
}

/// Ctrl-C or SIGTERM (systemd stop).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).unwrap_or_else(|e| fail(e));
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
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
            #[cfg(feature = "fdb")]
            let sup = supervisor::managed(&cfg).then(|| supervise(&cfg));
            #[cfg(not(feature = "fdb"))]
            let sup: Option<Supervisor> = None;
            run(cfg, sup, true).await;
        }
        #[cfg(feature = "fdb")]
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
        #[cfg(feature = "fdb")]
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
        #[cfg(feature = "fdb")]
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
        #[cfg(feature = "fdb")]
        Cmd::Backup { cmd } => backup(cmd).await,
        #[cfg(feature = "fdb")]
        Cmd::Restore {
            config,
            source,
            timestamp,
            version,
            orig_cluster_file,
            add_prefix,
        } => {
            let cfg = load(&config);
            require_fdb(&cfg);
            let cluster = cluster_arg(&cfg);
            let mut a = vec![
                s("start"),
                s("-r"),
                source,
                s("--dest-cluster-file"),
                cluster.clone(),
                s("-w"),
            ];
            if let Some(t) = timestamp {
                // fdbrestore maps a timestamp to a version with the metadata
                // of the backed-up database, not of the target.
                let orig = orig_cluster_file.unwrap_or(cluster);
                a.extend([s("--timestamp"), t, s("--orig-cluster-file"), orig]);
            }
            if let Some(v) = version {
                a.extend([s("-v"), v.to_string()]);
            }
            if let Some(p) = add_prefix {
                a.extend([s("--add-prefix"), p]);
            }
            supervisor::run_tool(&cfg, "fdbrestore", &a)
                .await
                .unwrap_or_else(|e| fail(e));
        }
        Cmd::Export { config, out } => {
            let cfg = load(&config);
            let store = zen_server::open_store(&cfg).unwrap_or_else(|e| fail(e));
            let f = std::fs::File::create(&out)
                .unwrap_or_else(|e| fail(format!("{}: {e}", out.display())));
            let backend = format!("{:?}", cfg.backend()).to_lowercase();
            let st = dump::export(store.as_ref(), &backend, f)
                .await
                .unwrap_or_else(|e| fail(e));
            println!("exported {} keys, {} bytes", st.keys, st.bytes);
            if st.inconsistent {
                eprintln!(
                    "zen-serve: warning: the export spans several read versions; \
                     stop the servers for a consistent export, or use `zen-serve backup`"
                );
            }
        }
        Cmd::Import {
            config,
            input,
            force,
        } => {
            let cfg = load(&config);
            let store = zen_server::open_store(&cfg).unwrap_or_else(|e| fail(e));
            let f = std::fs::File::open(&input)
                .unwrap_or_else(|e| fail(format!("{}: {e}", input.display())));
            let (h, st) = dump::import(store.as_ref(), f, force)
                .await
                .unwrap_or_else(|e| fail(e));
            println!(
                "imported {} keys, {} bytes (from {})",
                st.keys, st.bytes, h.backend
            );
        }
        Cmd::Migrate {
            config,
            from_data_dir,
            force,
        } => {
            let cfg = load(&config);
            let from = Embedded::open(from_data_dir.join("zen.redb"), Options::default())
                .unwrap_or_else(|e| fail(format!("{}: {e}", from_data_dir.display())));
            let to = zen_server::open_store(&cfg).unwrap_or_else(|e| fail(e));
            let st = dump::copy(&from, to.as_ref(), force)
                .await
                .unwrap_or_else(|e| fail(e));
            println!("migrated {} keys, {} bytes", st.keys, st.bytes);
        }
    }
}

#[cfg(feature = "fdb")]
fn cluster_arg(cfg: &Config) -> String {
    match cfg.cluster_file() {
        Some(p) => p.display().to_string(),
        None => "/etc/foundationdb/fdb.cluster".into(),
    }
}

#[cfg(feature = "fdb")]
fn require_fdb(cfg: &Config) {
    if cfg.backend() != Backend::Fdb {
        fail("this needs the FoundationDB backend ([storage] backend = \"fdb\")");
    }
}

#[cfg(feature = "fdb")]
fn s(x: &str) -> String {
    x.to_owned()
}

#[cfg(feature = "fdb")]
async fn backup(cmd: BackupCmd) {
    let (cfg, args) = match cmd {
        BackupCmd::Start {
            config,
            dest,
            snapshot_interval,
        } => {
            let cfg = load(&config);
            let mut a = vec![
                s("start"),
                s("-C"),
                cluster_arg(&cfg),
                s("-d"),
                dest,
                s("-z"),
            ];
            if let Some(i) = snapshot_interval {
                a.extend([s("-s"), i.to_string()]);
            }
            (cfg, a)
        }
        BackupCmd::Status { config } => {
            let cfg = load(&config);
            let a = vec![s("status"), s("-C"), cluster_arg(&cfg)];
            (cfg, a)
        }
        BackupCmd::Stop { config } => {
            let cfg = load(&config);
            let a = vec![s("discontinue"), s("-C"), cluster_arg(&cfg)];
            (cfg, a)
        }
        BackupCmd::Describe { config, dest } => {
            let cfg = load(&config);
            let a = vec![s("describe"), s("-C"), cluster_arg(&cfg), s("-d"), dest];
            (cfg, a)
        }
    };
    require_fdb(&cfg);
    supervisor::run_tool(&cfg, "fdbbackup", &args)
        .await
        .unwrap_or_else(|e| fail(e));
}

#[cfg(feature = "fdb")]
fn print_token(cfg: &Config) {
    let path = supervisor::cluster_file(cfg);
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| fail(format!("{}: {e} (not an init/join node)", path.display())));
    println!("join token: {}", supervisor::join_token(&contents));
}
