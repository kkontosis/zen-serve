//! The FoundationDB supervisor (DESIGN-2 §6, operations.md): zen-serve runs
//! its node's `fdbserver` (and `backup_agent`) processes itself, restarts
//! them when they exit, and raises the cluster's redundancy as nodes join.
//!
//! * `zen-serve init` creates a cluster: a new cluster file in `data_dir`,
//!   the processes, `configure new single ssd`.
//! * `zen-serve join <token>` adds a node: the token is the cluster file.
//! * `zen-serve serve` keeps supervising a node set up by either.
//!
//! The processes of one node share a machine id, so FoundationDB places
//! replicas on different nodes. The redundancy policy only raises the mode
//! (`single` → `double` at 3 nodes → `triple` at 5) and then runs
//! `coordinators auto`; one node acts, the one with the lowest process
//! address. Lowering redundancy is left to the operator (a lost node must
//! not reduce it).

use crate::config::Config;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zen_proto::ClusterStatus;

/// The managed cluster file of a node.
pub fn cluster_file(cfg: &Config) -> PathBuf {
    cfg.data_dir.join("fdb.cluster")
}

/// Whether this node runs FoundationDB processes (set up by init/join).
pub fn managed(cfg: &Config) -> bool {
    cluster_file(cfg).exists()
}

const SEARCH: &[&str] = &[
    "/opt/foundationdb/usr/sbin",
    "/opt/foundationdb/usr/bin",
    "/opt/foundationdb/usr/lib/foundationdb/backup_agent",
    "/usr/sbin",
    "/usr/bin",
    "/usr/lib/foundationdb",
    "/usr/lib/foundationdb/backup_agent",
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/local/libexec",
];

/// Find a FoundationDB binary: `fdb.bin_dir`, the standard locations, then
/// `PATH`.
pub fn find_bin(cfg: &Config, name: &str) -> Result<PathBuf, String> {
    let mut dirs: Vec<PathBuf> = cfg.fdb.bin_dir.iter().cloned().collect();
    if cfg.fdb.bin_dir.is_none() {
        dirs.extend(SEARCH.iter().map(PathBuf::from));
        if let Some(path) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&path));
        }
    }
    dirs.iter()
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            format!("{name} not found (set [fdb] bin_dir, or run scripts/install-fdb.sh)")
        })
}

fn tls_args(cfg: &Config) -> Vec<String> {
    match (&cfg.fdb.tls_cert, &cfg.fdb.tls_key, &cfg.fdb.tls_ca) {
        (Some(c), Some(k), Some(a)) => vec![
            "--tls-certificate-file".into(),
            c.display().to_string(),
            "--tls-key-file".into(),
            k.display().to_string(),
            "--tls-ca-file".into(),
            a.display().to_string(),
        ],
        _ => Vec::new(),
    }
}

fn tls_suffix(cfg: &Config) -> &'static str {
    if cfg.fdb.tls_cert.is_some() {
        ":tls"
    } else {
        ""
    }
}

fn public_ip(cfg: &Config) -> &str {
    cfg.fdb.public_ip.as_deref().unwrap_or(&cfg.fdb.listen_ip)
}

fn random_alnum(n: usize) -> String {
    const A: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("OS RNG");
    b.iter().map(|x| A[*x as usize % A.len()] as char).collect()
}

/// A new cluster file for a cluster whose first coordinator is this node.
pub fn new_cluster_file(cfg: &Config) -> String {
    format!(
        "zen:{}@{}:{}{}\n",
        random_alnum(16),
        public_ip(cfg),
        cfg.fdb.port,
        tls_suffix(cfg)
    )
}

/// The join token for a cluster file: base64url of its contents. It is not
/// a secret against hosts that can reach the cluster; protect the network
/// or use TLS (operations.md).
pub fn join_token(contents: &str) -> String {
    URL_SAFE_NO_PAD.encode(contents.trim())
}

/// The cluster file a join token stands for.
pub fn parse_join_token(token: &str) -> Result<String, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(token.trim())
        .map_err(|_| "bad join token")?;
    let s = String::from_utf8(bytes).map_err(|_| "bad join token")?;
    let (head, addrs) = s.split_once('@').ok_or("bad join token")?;
    if !head.contains(':') || addrs.is_empty() {
        return Err("bad join token".into());
    }
    Ok(format!("{s}\n"))
}

/// This node's machine id (16 hex digits), created once.
fn node_id(cfg: &Config) -> Result<String, String> {
    let path = cfg.data_dir.join("fdb").join("node-id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        return Ok(id.trim().to_owned());
    }
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("OS RNG");
    let id: String = b.iter().map(|x| format!("{x:02x}")).collect();
    std::fs::create_dir_all(path.parent().expect("parent")).map_err(|e| e.to_string())?;
    std::fs::write(&path, &id).map_err(|e| e.to_string())?;
    Ok(id)
}

/// Write the managed cluster file (`init` / `join`). Refuses to overwrite.
pub fn write_cluster_file(cfg: &Config, contents: &str) -> Result<(), String> {
    let path = cluster_file(cfg);
    if path.exists() {
        return Err(format!(
            "{} exists: this node is already set up (run `zen-serve serve`)",
            path.display()
        ));
    }
    std::fs::create_dir_all(&cfg.data_dir).map_err(|e| format!("data_dir: {e}"))?;
    std::fs::write(&path, contents).map_err(|e| format!("{}: {e}", path.display()))
}

/// One supervised child process.
struct Child {
    name: String,
    program: PathBuf,
    args: Vec<String>,
}

/// The running supervisor of one node.
pub struct Supervisor {
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    pids: Arc<Mutex<Vec<Option<u32>>>>,
}

impl Supervisor {
    /// Start this node's processes: `fdb.processes` × `fdbserver`, plus a
    /// `backup_agent` when `[backup] agents`, plus the redundancy policy.
    pub fn start(cfg: &Config) -> Result<Self, String> {
        let cluster = cluster_file(cfg);
        let id = node_id(cfg)?;
        let fdbserver = find_bin(cfg, "fdbserver")?;
        let mut children = Vec::new();
        for i in 0..cfg.fdb.processes {
            let port = cfg.fdb.port + i;
            let dir = cfg.data_dir.join("fdb").join(port.to_string());
            let logs = cfg.data_dir.join("fdb").join("log");
            for d in [&dir, &logs] {
                std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
            }
            let mut args = vec![
                "-C".into(),
                cluster.display().to_string(),
                "-p".into(),
                format!("{}:{port}{}", public_ip(cfg), tls_suffix(cfg)),
                "-l".into(),
                format!("{}:{port}{}", cfg.fdb.listen_ip, tls_suffix(cfg)),
                "-d".into(),
                dir.display().to_string(),
                "-L".into(),
                logs.display().to_string(),
                "-i".into(),
                id.clone(),
            ];
            args.extend(tls_args(cfg));
            children.push(Child {
                name: format!("fdbserver:{port}"),
                program: fdbserver.clone(),
                args,
            });
        }
        if cfg.backup.agents {
            let logs = cfg.data_dir.join("fdb").join("log");
            let mut args = vec![
                "-C".into(),
                cluster.display().to_string(),
                "--log".into(),
                "--logdir".into(),
                logs.display().to_string(),
            ];
            args.extend(tls_args(cfg));
            children.push(Child {
                name: "backup_agent".into(),
                program: find_bin(cfg, "backup_agent")?,
                args,
            });
        }
        let (stop, stop_rx) = watch::channel(false);
        let pids = Arc::new(Mutex::new(vec![None; children.len()]));
        let mut tasks: Vec<JoinHandle<()>> = children
            .into_iter()
            .enumerate()
            .map(|(i, c)| tokio::spawn(supervise(c, i, pids.clone(), stop_rx.clone())))
            .collect();
        if cfg.fdb.auto_redundancy {
            tasks.push(tokio::spawn(redundancy_policy(cfg.clone(), stop_rx)));
        }
        Ok(Supervisor { stop, tasks, pids })
    }

    /// Process ids of the running children (tests).
    pub fn pids(&self) -> Vec<Option<u32>> {
        self.pids.lock().expect("pids lock").clone()
    }

    /// Stop every child and wait for them to exit.
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        for t in self.tasks {
            let _ = t.await;
        }
    }
}

async fn supervise(
    c: Child,
    slot: usize,
    pids: Arc<Mutex<Vec<Option<u32>>>>,
    mut stop: watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if *stop.borrow() {
            return;
        }
        let started = Instant::now();
        let spawned = Command::new(&c.program)
            .args(&c.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(ch) => ch,
            Err(e) => {
                tracing::error!(process = %c.name, error = %e, "spawn failed");
                if sleep_or_stop(&mut stop, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        pids.lock().expect("pids lock")[slot] = child.id();
        tracing::info!(process = %c.name, pid = child.id(), "started");
        if let Some(err) = child.stderr.take() {
            let name = c.name.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    tracing::warn!(process = %name, "{l}");
                }
            });
        }
        let stopping = tokio::select! {
            status = child.wait() => {
                tracing::warn!(process = %c.name, ?status, "exited; restarting");
                false
            }
            _r = stop.wait_for(|s| *s) => true,
        };
        if stopping {
            stop_child(&c.name, &mut child).await;
        }
        pids.lock().expect("pids lock")[slot] = None;
        if stopping {
            return;
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        if sleep_or_stop(&mut stop, backoff).await {
            return;
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// How long a child gets to exit on SIGTERM before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// Stop a child the way `fdbmonitor` does: SIGTERM, a bounded wait, then
/// SIGKILL.
async fn stop_child(name: &str, child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let pid = Pid::from_raw(pid as i32);
        if kill(pid, Signal::SIGTERM).is_ok()
            && tokio::time::timeout(STOP_GRACE, child.wait()).await.is_ok()
        {
            tracing::info!(process = %name, "stopped");
            return;
        }
        tracing::warn!(process = %name, "did not stop on SIGTERM; killing");
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Sleep for `d`; true if asked to stop meanwhile.
async fn sleep_or_stop(stop: &mut watch::Receiver<bool>, d: Duration) -> bool {
    let stopped = tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _r = stop.wait_for(|s| *s) => true,
    };
    stopped || *stop.borrow()
}

/// Run `fdbcli --exec <cmd>` against the node's cluster file.
pub async fn fdbcli(cfg: &Config, cluster: &Path, cmd: &str) -> Result<String, String> {
    let out = Command::new(find_bin(cfg, "fdbcli")?)
        .arg("-C")
        .arg(cluster)
        .args(tls_args(cfg))
        .args(["--timeout", "30", "--exec", cmd])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("fdbcli: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!(
            "fdbcli {cmd}: {}{}",
            text.trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Run a FoundationDB tool (`fdbbackup`, `fdbrestore`) with the node's TLS
/// options, passing its output through.
pub async fn run_tool(cfg: &Config, name: &str, args: &[String]) -> Result<(), String> {
    let status = Command::new(find_bin(cfg, name)?)
        .args(args)
        .args(tls_args(cfg))
        .stdin(Stdio::null())
        .status()
        .await
        .map_err(|e| format!("{name}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{name} failed ({status})"))
    }
}

/// `configure new single ssd`, retried until the new processes answer.
pub async fn configure_new(cfg: &Config) -> Result<(), String> {
    let cluster = cluster_file(cfg);
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match fdbcli(cfg, &cluster, "configure new single ssd").await {
            Ok(_) => return Ok(()),
            Err(e) if e.contains("already exists") => return Ok(()),
            Err(e) if Instant::now() > deadline => return Err(e),
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

/// `status json`.
pub async fn status_json(cfg: &Config, cluster: &Path) -> Result<Value, String> {
    let text = fdbcli(cfg, cluster, "status json").await?;
    serde_json::from_str(&text).map_err(|e| format!("status json: {e}"))
}

/// Summarize `status json`.
pub fn summarize(s: &Value) -> ClusterStatus {
    let c = &s["cluster"];
    let count = |v: &Value| v.as_object().map_or(0, |m| m.len() as u32);
    let mut messages: Vec<String> = c["messages"]
        .as_array()
        .into_iter()
        .chain(s["client"]["messages"].as_array())
        .flatten()
        .filter_map(|m| m["description"].as_str().or(m["name"].as_str()))
        .map(String::from)
        .collect();
    messages.dedup();
    ClusterStatus {
        backend: "fdb".into(),
        available: s["client"]["database_status"]["available"]
            .as_bool()
            .unwrap_or(false),
        healthy: s["client"]["database_status"]["healthy"]
            .as_bool()
            .unwrap_or(false),
        redundancy: c["configuration"]["redundancy_mode"]
            .as_str()
            .map(String::from),
        machines: count(&c["machines"]),
        processes: count(&c["processes"]),
        coordinators: s["client"]["coordinators"]["coordinators"]
            .as_array()
            .map_or(0, |a| a.len() as u32),
        messages,
    }
}

/// The mode the policy wants for `machines` nodes.
pub fn desired_mode(machines: u32) -> &'static str {
    match machines {
        0..=2 => "single",
        3..=4 => "double",
        _ => "triple",
    }
}

fn mode_rank(m: &str) -> u8 {
    match m {
        "single" => 1,
        "double" => 2,
        "triple" => 3,
        _ => 0,
    }
}

async fn redundancy_policy(cfg: Config, mut stop: watch::Receiver<bool>) {
    let cluster = cluster_file(&cfg);
    let mine: Vec<String> = (0..cfg.fdb.processes)
        .map(|i| format!("{}:{}", public_ip(&cfg), cfg.fdb.port + i))
        .collect();
    loop {
        if sleep_or_stop(&mut stop, Duration::from_secs(30)).await {
            return;
        }
        let s = match status_json(&cfg, &cluster).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, "redundancy policy: no status");
                continue;
            }
        };
        // One node acts: the one owning the lowest process address.
        let lowest = s["cluster"]["processes"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.values())
            .filter_map(|p| p["address"].as_str())
            .map(|a| a.trim_end_matches(":tls").to_owned())
            .min();
        if !lowest.is_some_and(|l| mine.contains(&l)) {
            continue;
        }
        let sum = summarize(&s);
        let want = desired_mode(sum.machines);
        let have = sum.redundancy.as_deref().unwrap_or("");
        if mode_rank(want) > mode_rank(have) {
            tracing::info!(
                from = have,
                to = want,
                machines = sum.machines,
                "raising redundancy"
            );
            if let Err(e) = fdbcli(&cfg, &cluster, &format!("configure {want}")).await {
                tracing::warn!(error = %e, "configure failed");
                continue;
            }
        }
        let want_coord = match mode_rank(want).max(mode_rank(have)) {
            3 => 5,
            2 => 3,
            _ => 1,
        };
        if sum.coordinators < want_coord && sum.machines > sum.coordinators {
            tracing::info!(coordinators = sum.coordinators, "coordinators auto");
            if let Err(e) = fdbcli(&cfg, &cluster, "coordinators auto").await {
                tracing::warn!(error = %e, "coordinators auto failed");
            }
        }
    }
}
