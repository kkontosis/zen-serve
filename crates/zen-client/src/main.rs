//! `zen-client`: sync and checkpoint a device's view of an fs, compare pins
//! out of band, and read or write KV values (docs/CLIENT.md §13).
//!
//! Every command signs in with the device key, attaches integrity with the
//! pins file, unlocks the fs with the device's keyslot and syncs first. An
//! alarm prints as JSON on stderr and exits 3; other errors exit 1.

use clap::{Parser, Subcommand};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use zen_client::cli::{ConnectArgs, Connected, exit_code, print_error};
use zen_client::{Error, IntegrityOptions, RangeOptions, hex, unhex};

#[derive(Parser)]
#[command(name = "zen-client", version, about = "A native zen-serve client")]
struct Cli {
    #[command(flatten)]
    connect: ConnectArgs,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Verify every device's newest checkpoint and raise the pins.
    Sync,
    /// Sync, then write a checkpoint of this device.
    Checkpoint,
    /// Exported pins (spec/formats.md §14.5).
    Pins {
        #[command(subcommand)]
        op: PinsOp,
    },
    /// KV values by path (`/`-separated UTF-8 elements).
    Kv {
        #[command(subcommand)]
        op: KvOp,
    },
    /// The fs header: signed version, epoch, keyslots.
    Header,
}

#[derive(Subcommand)]
enum PinsOp {
    /// Write this device's pins: raw bytes to FILE, or hex on stdout.
    Export {
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Compare another client's pins (a file of raw bytes or hex; `-`: stdin).
    Compare { file: String },
}

#[derive(Subcommand)]
enum KvOp {
    /// Print a value (as UTF-8 when it is, else hex).
    Get { path: String },
    /// Write a value (UTF-8).
    Set { path: String, value: String },
    /// List the values under a path: stored key (hex) and value.
    List { path: String },
}

fn elements(path: &str) -> Vec<&[u8]> {
    path.split('/')
        .filter(|e| !e.is_empty())
        .map(str::as_bytes)
        .collect()
}

fn printable(v: &[u8]) -> String {
    match std::str::from_utf8(v) {
        Ok(s) => s.to_string(),
        Err(_) => hex(v),
    }
}

fn read_pins(file: &str) -> Result<Vec<u8>, Error> {
    let mut b = Vec::new();
    if file == "-" {
        std::io::stdin().read_to_end(&mut b)?;
    } else {
        b = std::fs::read(file).map_err(|e| Error::Io(format!("{file}: {e}")))?;
    }
    // Hex text (as `pins export` prints it) or the raw record.
    match std::str::from_utf8(&b).ok().and_then(unhex) {
        Some(raw) => Ok(raw),
        None => Ok(b),
    }
}

async fn run(cli: Cli) -> Result<u8, Error> {
    let Connected {
        integrity, fs: u, ..
    } = cli.connect.connect(IntegrityOptions::default()).await?;
    let mut out = std::io::stdout().lock();
    let mut code = 0;
    match cli.command {
        Command::Sync => {}
        Command::Checkpoint => {
            if let Some(seq) = integrity.checkpoint_always().await? {
                writeln!(out, "{seq}")?;
            }
        }
        Command::Pins { op } => match op {
            PinsOp::Export { out: file } => {
                let b = integrity.export_pins()?;
                match file {
                    Some(f) => std::fs::write(&f, b)?,
                    None => writeln!(out, "{}", hex(&b))?,
                }
            }
            PinsOp::Compare { file } => {
                let alarms = integrity.compare_pins(&read_pins(&file)?).await?;
                for a in &alarms {
                    eprintln!("{}", a.to_json());
                }
                if !alarms.is_empty() {
                    code = 3;
                }
            }
        },
        Command::Kv { op } => match op {
            KvOp::Get { path } => match u.get(&u.key(&elements(&path))).await? {
                Some(e) => writeln!(out, "{}", printable(&e.value))?,
                None => {
                    eprintln!("{path}: absent");
                    code = 1;
                }
            },
            KvOp::Set { path, value } => u.set(&u.key(&elements(&path)), value.as_bytes()).await?,
            KvOp::List { path } => {
                let els = elements(&path);
                let prefix = if els.is_empty() {
                    Vec::new()
                } else {
                    u.key(&els)
                };
                for e in u.range_prefix(&prefix, RangeOptions::default()).await? {
                    writeln!(out, "{} {}", hex(&e.key), printable(&e.value))?;
                }
            }
        },
        Command::Header => {
            let s = u.stored();
            let j = serde_json::json!({
                "signed": s.signed.map(|x| x.version),
                "epoch": s.header.current_epoch,
                "slots": s.header.slots.len(),
            });
            writeln!(out, "{j}")?;
        }
    }
    integrity.close().await?;
    Ok(code)
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            print_error("zen-client", &e);
            ExitCode::from(exit_code(&e))
        }
    }
}
