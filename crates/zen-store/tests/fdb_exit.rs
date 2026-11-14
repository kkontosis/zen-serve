//! A process that used the FoundationDB backend stops the client network
//! as it exits (`fdb.rs`, `at_exit`), as `foundationdb` requires: left
//! running, the network thread can fault while the process tears down
//! (milestone 9 step 3c). Needs `libfdb_c` but no cluster: the child opens a
//! cluster file naming an address where nothing listens, so its reads are
//! still waiting when it exits. A crash at exit fails the status check; an
//! exit that left the network running lacks the hook's line.
#![cfg(all(feature = "fdb", target_os = "linux", target_env = "gnu"))]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use zen_store::Storage;
use zen_store::fdb::Fdb;

/// Set in the child: the cluster file it opens.
const CHILD: &str = "ZEN_TEST_FDB_EXIT_CHILD";
/// The hook's trace switch (`fdb.rs`).
const TRACE: &str = "ZEN_TEST_FDB_EXIT_TRACE";
const STOPPED: &str = "zen-store: the FoundationDB network stopped";

/// The child: reads in flight on an unreachable cluster, then the test
/// returns and the harness exits the process.
fn child(cluster: &str) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let db = Fdb::open(Some(cluster)).expect("open fdb");
        for i in 0..4u8 {
            let db = db.clone();
            tokio::spawn(async move {
                if let Ok(mut t) = db.begin(None).await {
                    let _ = t.get(&[i]).await;
                }
            });
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    });
}

#[test]
fn exit_stops_the_network() {
    if let Ok(cluster) = std::env::var(CHILD) {
        return child(&cluster);
    }
    let dir = tempfile::tempdir().unwrap();
    let cluster = dir.path().join("fdb.cluster");
    std::fs::write(&cluster, "zen:test@127.0.0.1:1\n").unwrap();

    let mut p = Command::new(std::env::current_exe().unwrap())
        .args(["exit_stops_the_network", "--exact", "--nocapture"])
        .env(CHILD, &cluster)
        .env(TRACE, "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(s) = p.try_wait().unwrap() {
            break s;
        }
        if start.elapsed() > Duration::from_secs(10) {
            p.kill().unwrap();
            panic!("the child didn't exit within 10 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut err = String::new();
    p.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert!(
        status.success(),
        "the child exited with {status}; its stderr:\n{err}"
    );
    assert!(
        err.contains(STOPPED),
        "the exit hook didn't stop the network; the child's stderr:\n{err}"
    );
}
