//! Accepting relayed connections (svc.md §3, steps 1 and 2).

use super::{Service, conn, node};
use crate::config::SvcMode;
use crate::state::Shared;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};

/// Bind the `tcp` services' listeners of `cfg` (before the server starts).
pub async fn bind(cfg: &crate::config::Config) -> Result<Vec<(u32, Vec<u8>, TcpListener)>, String> {
    let mut out = Vec::new();
    for s in &cfg.service {
        if let (SvcMode::Tcp, Some(addr)) = (s.mode, s.listen) {
            let l = TcpListener::bind(addr)
                .await
                .map_err(|e| format!("service {:?}: bind {addr}: {e}", s.name))?;
            out.push((s.fs, s.id.clone(), l));
        }
    }
    Ok(out)
}

/// Take the node lease, then start the node's relay tasks: the lease
/// renewer, the work loop and an accept loop per listener.
pub async fn start(st: &Shared, listeners: Vec<(u32, Vec<u8>, TcpListener)>) -> Result<(), String> {
    if st.cfg.service.is_empty() {
        return Ok(());
    }
    node::renew(st, true)
        .await
        .map_err(|e| format!("node lease: {}", e.message))?;
    st.svc
        .keep(tokio::spawn(node::renewer(st.clone())).abort_handle());
    st.svc
        .keep(tokio::spawn(node::work(st.clone())).abort_handle());
    for (fs, id, l) in listeners {
        let s = st.svc.service(fs, &id).expect("configured").clone();
        *s.addr.lock().expect("addr lock") = l.local_addr().ok();
        tracing::info!(service = %s.cfg.name, addr = ?l.local_addr().ok(), "relaying");
        st.svc
            .keep(tokio::spawn(accept_loop(st.clone(), s, l)).abort_handle());
    }
    Ok(())
}

async fn accept_loop(st: Shared, svc: Arc<Service>, listener: TcpListener) {
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(service = %svc.cfg.name, error = %e, "accept");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // Admission, before anything is written.
        if !svc.take(peer.ip(), Instant::now()) || !svc.open() {
            tracing::debug!(service = %svc.cfg.name, %peer, "refused: over a limit");
            reset(sock);
            continue;
        }
        let _ = sock.set_nodelay(true);
        conn::spawn(
            &st,
            svc.clone(),
            sock,
            zen_proto::SvcPeer {
                addr: Some(peer.to_string()),
                ..Default::default()
            },
        );
    }
}

/// Close with a reset (`SO_LINGER` 0).
pub fn reset(sock: TcpStream) {
    let _ = sock.set_zero_linger();
    drop(sock);
}
