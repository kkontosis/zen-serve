//! The FoundationDB supervisor: `init` in a tempdir gives a working
//! single-node cluster, and a killed `fdbserver` is restarted. Runs with
//! `ZEN_TEST_BACKEND=fdb` (needs the FoundationDB binaries).

#![cfg(feature = "fdb")]

mod common;

use std::time::{Duration, Instant};
use zen_server::config::Config;
use zen_server::supervisor::{self, Supervisor};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test(flavor = "multi_thread")]
async fn init_runs_a_cluster_and_restarts_processes() {
    if !common::on_fdb() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::with_data_dir(dir.path().join("data"));
    cfg.fdb.port = free_port();
    cfg.fdb.auto_redundancy = false;
    supervisor::write_cluster_file(&cfg, &supervisor::new_cluster_file(&cfg)).unwrap();
    assert!(supervisor::write_cluster_file(&cfg, "x:y@127.0.0.1:1").is_err());
    let token =
        supervisor::join_token(&std::fs::read_to_string(supervisor::cluster_file(&cfg)).unwrap());
    assert!(supervisor::parse_join_token(&token).unwrap().contains('@'));

    let sup = Supervisor::start(&cfg).unwrap();
    supervisor::configure_new(&cfg).await.unwrap();
    let s = zen_server::cluster_status(&cfg).await;
    assert_eq!(s.backend, "fdb");
    assert!(s.available, "{s:?}");
    assert_eq!(s.redundancy.as_deref(), Some("single"));

    // The API serves on the new cluster (the backend follows the managed
    // cluster file).
    cfg.listen = "127.0.0.1:0".parse().unwrap();
    let server = zen_server::start(cfg.clone()).await.unwrap();
    assert!(server.claim_token.is_some());
    server.abort();

    let pid = sup.pids()[0].expect("running");
    std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match sup.pids()[0] {
            Some(p) if p != pid => break,
            _ => {
                assert!(Instant::now() < deadline, "fdbserver restarted");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while !zen_server::cluster_status(&cfg).await.available {
        assert!(Instant::now() < deadline, "cluster available again");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    sup.shutdown().await;
}

#[test]
fn redundancy_policy_modes() {
    assert_eq!(supervisor::desired_mode(1), "single");
    assert_eq!(supervisor::desired_mode(2), "single");
    assert_eq!(supervisor::desired_mode(3), "double");
    assert_eq!(supervisor::desired_mode(4), "double");
    assert_eq!(supervisor::desired_mode(5), "triple");
}
