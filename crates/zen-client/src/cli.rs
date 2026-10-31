//! What the native command-line tools share (`zen-client`, `zen-ublk`):
//! the options that name a server, a device and an fs, and the connect
//! sequence behind them (docs/CLIENT.md §13).

use crate::{
    Client, ClientOptions, DeviceKey, Error, Integrity, IntegrityOptions, Result, Session,
    TlsOptions, UnlockedFs,
};
use std::path::PathBuf;

/// The options that name a server, a device and an fs.
#[derive(clap::Args, Debug, Clone)]
pub struct ConnectArgs {
    /// Server URL (http:// or https://).
    #[arg(long)]
    pub url: String,
    /// Device key file: JSON {identity_seed, device_seed, cert} (hex), as zen-mount's.
    #[arg(long)]
    pub device_key: PathBuf,
    /// The fs id.
    #[arg(long)]
    pub fs: u32,
    /// The pins file (default: the device key file's path + `.pins-<fs>`).
    #[arg(long)]
    pub pins: Option<PathBuf>,
    /// The origin to sign (default: the URL's).
    #[arg(long)]
    pub origin: Option<String>,
    /// Trust only this CA (PEM) for the server's certificate.
    #[arg(long)]
    pub ca: Option<PathBuf>,
}

/// A device signed in, with integrity attached and the fs unlocked.
pub struct Connected {
    /// The device key.
    pub key: DeviceKey,
    /// The session.
    pub session: Session,
    /// Integrity, with the pins file, synced.
    pub integrity: Integrity,
    /// The fs, unlocked with the device's keyslot.
    pub fs: UnlockedFs,
}

impl ConnectArgs {
    /// The device key file's path with `suffix` appended.
    pub fn beside_key(&self, suffix: &str) -> PathBuf {
        let mut p = self.device_key.clone().into_os_string();
        p.push(suffix);
        p.into()
    }

    /// The pins file: `--pins`, or the device key's path + `.pins-<fs>`.
    pub fn pins_path(&self) -> PathBuf {
        self.pins
            .clone()
            .unwrap_or_else(|| self.beside_key(&format!(".pins-{}", self.fs)))
    }

    /// Sign in with the device key, attach integrity (the device signs,
    /// pins in [`ConnectArgs::pins_path`]; `signer` and `pins` of `opts`
    /// are set here), unlock the fs with the device's keyslot, and sync.
    pub async fn connect(&self, opts: IntegrityOptions) -> Result<Connected> {
        let key = DeviceKey::load(&self.device_key)?;
        let tls = TlsOptions {
            ca: match &self.ca {
                Some(p) => {
                    Some(std::fs::read(p).map_err(|e| Error::Io(format!("{}: {e}", p.display())))?)
                }
                None => None,
            },
        };
        let client = Client::connect(
            &self.url,
            ClientOptions {
                origin: self.origin.clone(),
                tls,
            },
        )?;
        let session = client.sign_in_device(&key).await?;
        let fs = session.fs(self.fs);
        let integrity = fs
            .integrity(IntegrityOptions {
                signer: Some(key.clone()),
                pins: Some(self.pins_path()),
                ..opts
            })
            .await?;
        let fs = fs.unlock_device(&key.device).await?;
        integrity.sync().await?;
        Ok(Connected {
            key,
            session,
            integrity,
            fs,
        })
    }
}

/// The exit code of a failed command: 3 for an integrity alarm, else 1.
pub fn exit_code(e: &Error) -> u8 {
    if e.alarm().is_some() { 3 } else { 1 }
}

/// Print a failed command's error on stderr: an alarm as JSON, anything
/// else as `<tool>: <error>`.
pub fn print_error(tool: &str, e: &Error) {
    match e.alarm() {
        Some(a) => eprintln!("{}", a.to_json()),
        None => eprintln!("{tool}: {e}"),
    }
}
