//! zen-serve: the keyless E2EE storage server (spec/api.md).
//!
//! The server holds no keys. It stores sealed values and events, enforces the
//! signed ACL, and runs commits, the event log and consumer groups as
//! transactions on a [`zen_store::Storage`] backend.
#![forbid(unsafe_code)]

pub mod acl;
pub mod auth;
pub mod cbor;
pub mod commit;
pub mod config;
pub mod consume;
pub mod dump;
pub mod eph;
pub mod error;
pub mod ids;
pub mod keys;
pub mod kv;
pub mod log;
pub mod state;
pub mod statics;
pub mod stream;
pub mod supervisor;
pub mod tree;
mod txn;

use crate::cbor::Cbor;
use crate::config::Config;
use crate::state::{AppState, Shared};
use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, Method, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{AllowOrigin, CorsLayer};
use zen_proto::{API_VERSION, Info, Limits};
use zen_store::Storage;
use zen_store::embedded::{Embedded, Options};
use zen_store::prefixed::Prefixed;

/// `GET /v1/info`.
async fn info(State(st): State<Shared>) -> Cbor<Info> {
    let l = &st.cfg.limits;
    Cbor(Info {
        server: concat!("zen-serve/", env!("CARGO_PKG_VERSION")).into(),
        api: API_VERSION,
        suites: vec![1],
        formats: vec![1],
        features: ["kv", "log", "consume", "ephemeral", "static", "fs"]
            .map(String::from)
            .to_vec(),
        cross_origin_isolation: st.cfg.cross_origin_isolation,
        claimed: st.acl().version > 0,
        time_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        limits: Limits {
            max_key_bytes: l.max_key_bytes,
            max_value_bytes: l.max_value_bytes,
            max_envelope_bytes: l.max_envelope_bytes,
            max_commit_bytes: l.max_commit_bytes,
            max_commit_ops: l.max_commit_ops,
            max_range_items: l.max_range_items,
            max_range_bytes: l.max_range_bytes,
            idempotency_ttl_secs: l.idempotency_ttl_secs,
            session_ttl_secs: l.session_ttl_secs,
            crdt_max_skew_ms: l.crdt_max_skew_ms,
            crdt_horizon_secs: l.crdt_horizon_secs,
            crdt_max_redo: l.crdt_max_redo,
            crdt_max_depth: l.crdt_max_depth,
            chunk_grace_secs: l.chunk_grace_secs,
            claim_ttl_ms: l.claim_ttl_ms,
            ephemeral_ttl_secs: l.ephemeral_ttl_secs,
            max_groups_per_topic: l.max_groups_per_topic,
        },
    })
}

/// `POST /v1/admin/status`: storage health, admins only.
async fn admin_status(
    State(st): State<Shared>,
    caller: auth::Caller,
    Cbor(_): Cbor<zen_proto::Empty>,
) -> error::ApiResult<Cbor<zen_proto::ClusterStatus>> {
    if !caller.is_admin() {
        return Err(error::forbidden("admins only"));
    }
    Ok(Cbor(cluster_status(&st.cfg).await))
}

/// Storage health for `zen-serve status` and `/v1/admin/status`.
pub async fn cluster_status(cfg: &Config) -> zen_proto::ClusterStatus {
    match (cfg.backend(), cfg.cluster_file()) {
        (config::Backend::Fdb, Some(file)) => match supervisor::status_json(cfg, &file).await {
            Ok(s) => supervisor::summarize(&s),
            Err(e) => zen_proto::ClusterStatus {
                backend: "fdb".into(),
                messages: vec![e],
                ..Default::default()
            },
        },
        (config::Backend::Fdb, None) => zen_proto::ClusterStatus {
            backend: "fdb".into(),
            messages: vec!["no cluster file".into()],
            ..Default::default()
        },
        (config::Backend::Embedded, _) => zen_proto::ClusterStatus {
            backend: "embedded".into(),
            available: true,
            healthy: true,
            machines: 1,
            ..Default::default()
        },
    }
}

async fn isolation_headers(State(st): State<Shared>, req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    if st.cfg.cross_origin_isolation {
        let h = resp.headers_mut();
        h.insert(
            "cross-origin-opener-policy",
            HeaderValue::from_static("same-origin"),
        );
        h.insert(
            "cross-origin-embedder-policy",
            HeaderValue::from_static("require-corp"),
        );
        h.insert(
            "cross-origin-resource-policy",
            HeaderValue::from_static("same-origin"),
        );
    }
    resp
}

/// The HTTP router.
pub fn router(st: Shared) -> Router {
    let limit = st.cfg.limits.max_commit_bytes as usize + 1_000_000;
    // Requests that need no session get bodies no larger than they need:
    // a sign-in is ~12 KB of identity, certificate and signature; a signed
    // ACL of a few hundred members fits in 1 MiB.
    let auth_limit = DefaultBodyLimit::max(16 * 1024);
    let acl_limit = DefaultBodyLimit::max(1024 * 1024);
    let mut r = Router::new()
        .route("/v1/info", get(info))
        .route(
            "/v1/auth/challenge",
            post(auth::challenge).layer(auth_limit),
        )
        .route("/v1/auth/session", post(auth::session).layer(auth_limit))
        .route("/v1/auth/logout", post(auth::logout).layer(auth_limit))
        .route("/v1/acl/put", post(acl::put).layer(acl_limit))
        .route("/v1/acl/get", post(acl::get))
        .route("/v1/fs/list", post(acl::fs_list))
        .route("/v1/fs/header/get", post(acl::header_get))
        .route("/v1/fs/header/put", post(acl::header_put))
        .route("/v1/grv", post(kv::grv))
        .route("/v1/kv/get", post(kv::get))
        .route("/v1/kv/range", post(kv::range))
        .route("/v1/commit", post(commit::commit))
        .route("/v1/log/append", post(commit::append))
        .route("/v1/log/read", post(log::read))
        .route("/v1/consume/groups", post(consume::create_group))
        .route("/v1/consume/lease", post(consume::lease))
        .route("/v1/consume/release", post(consume::release))
        .route("/v1/consume/next", post(consume::next))
        .route("/v1/consume/nack", post(consume::nack))
        .route("/v1/consume/cursor", post(consume::cursor))
        .route("/v1/consume/dlq/list", post(consume::dlq_list))
        .route("/v1/consume/dlq/retry", post(consume::dlq_retry))
        .route("/v1/consume/dlq/drop", post(consume::dlq_drop))
        .route("/v1/fs/tree/list", post(tree::tree_list))
        .route("/v1/fs/tree/get", post(tree::tree_get))
        .route("/v1/fs/tree/children", post(tree::tree_children))
        .route("/v1/fs/tree/changes", post(tree::tree_changes))
        .route("/v1/fs/tree/chain", post(tree::tree_chain))
        .route("/v1/fs/file/get", post(tree::file_get))
        .route("/v1/fs/chunks/get", post(tree::chunks_get))
        .route("/v1/admin/status", post(admin_status))
        .route("/v1/stream", get(stream::ws))
        .fallback(statics::fallback)
        .layer(DefaultBodyLimit::max(limit))
        .layer(middleware::from_fn_with_state(
            st.clone(),
            isolation_headers,
        ));
    if !st.cfg.cors_origins.is_empty() {
        let origins: Vec<HeaderValue> = st
            .cfg
            .cors_origins
            .iter()
            .filter_map(|o| HeaderValue::from_str(o).ok())
            .collect();
        r = r.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods([Method::GET, Method::POST])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );
    }
    r.with_state(st)
}

/// Load or create the claim token while no ACL exists (api.md §4.1).
fn claim_token(cfg: &Config) -> std::io::Result<String> {
    let path = cfg.data_dir.join("claim-token");
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_owned();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let t = URL_SAFE_NO_PAD.encode(ids::random32());
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    opts.open(&path)?.write_all(t.as_bytes())?;
    Ok(t)
}

/// Background housekeeping (G13): expired idempotency records, sessions,
/// consumed challenges and the ephemeral ring. Every node runs it; each
/// sweep is an idempotent transaction.
async fn sweeper(st: Shared) {
    let period = Duration::from_secs(st.cfg.limits.sweep_interval_secs.max(1));
    loop {
        tokio::time::sleep(period).await;
        if let Err(e) = sweep_once(&st).await {
            tracing::warn!(error = %e.message, "sweep failed");
        }
    }
}

/// One sweeper pass.
pub async fn sweep_once(st: &Shared) -> error::ApiResult<()> {
    let now = st.store.now_version().await?;
    let n = commit::sweep(st, now).await?;
    if n > 0 {
        tracing::debug!(removed = n, "expired idempotency records");
    }
    let n = auth::sweep(st).await?;
    if n > 0 {
        tracing::debug!(removed = n, "expired sessions and challenges");
    }
    tree::sweep(st, now).await?;
    let cutoff = now.saturating_sub(st.cfg.limits.ephemeral_ttl_secs * zen_store::VERSIONS_PER_SEC);
    for f in &st.cfg.fs {
        st.eph.sweep(f.id, cutoff).await?;
    }
    Ok(())
}

/// Open the configured storage backend.
pub fn open_store(cfg: &Config) -> Result<Arc<dyn Storage>, String> {
    let store: Arc<dyn Storage> = match cfg.backend() {
        config::Backend::Embedded => {
            std::fs::create_dir_all(&cfg.data_dir).map_err(|e| format!("data_dir: {e}"))?;
            Arc::new(
                Embedded::open(cfg.data_dir.join("zen.redb"), Options::default())
                    .map_err(|e| format!("open database: {e}"))?,
            )
        }
        #[cfg(feature = "fdb")]
        config::Backend::Fdb => {
            if let (Some(cert), Some(key), Some(ca)) =
                (&cfg.fdb.tls_cert, &cfg.fdb.tls_key, &cfg.fdb.tls_ca)
            {
                zen_store::fdb::set_tls(zen_store::fdb::Tls {
                    cert: cert.display().to_string(),
                    key: key.display().to_string(),
                    ca: ca.display().to_string(),
                });
            }
            let file = cfg.cluster_file();
            let file = file.as_ref().map(|p| p.to_string_lossy().into_owned());
            Arc::new(
                zen_store::fdb::Fdb::open(file.as_deref())
                    .map_err(|e| format!("open FoundationDB: {e}"))?,
            )
        }
        #[cfg(not(feature = "fdb"))]
        config::Backend::Fdb => {
            return Err("this zen-serve was built without the fdb feature".into());
        }
    };
    Ok(if cfg.storage.key_prefix.is_empty() {
        store
    } else {
        Arc::new(Prefixed::new(store, cfg.storage.key_prefix.as_bytes()))
    })
}

/// A running server.
pub struct Server {
    /// Bound address.
    pub addr: SocketAddr,
    /// Shared state.
    pub state: Shared,
    /// The claim token, while unclaimed.
    pub claim_token: Option<String>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    /// Stop serving.
    pub fn abort(&self) {
        self.task.abort();
    }
}

/// Open storage, load the ACL, bind and serve in the background.
pub async fn start(cfg: Config) -> Result<Server, String> {
    cfg.validate()?;
    std::fs::create_dir_all(&cfg.data_dir).map_err(|e| format!("data_dir: {e}"))?;
    if cfg.public_origins.is_empty() {
        tracing::warn!(
            "public_origins is empty: session origins are derived from the Host header, \
             which a relaying server controls (api.md §3.3); set public_origins in production"
        );
    }
    let store = open_store(&cfg)?;
    let challenge_key = auth::challenge_key(store.as_ref())
        .await
        .map_err(|e| format!("challenge key: {}", e.message))?;
    let is_fs = |fs| cfg.has_fs(fs);
    let acl = acl::load(store.as_ref(), &is_fs)
        .await
        .map_err(|e| format!("load ACL: {}", e.message))?;
    let claim = if acl.version == 0 {
        let t = claim_token(&cfg).map_err(|e| format!("claim token: {e}"))?;
        // The one secret we print on purpose: it pins the first admin.
        eprintln!("zen-serve: unclaimed; claim token: {t}");
        Some(t)
    } else {
        // Claimed, perhaps through another node while this one was down.
        state::remove_claim_token(&cfg.data_dir);
        None
    };
    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .map_err(|e| format!("bind {}: {e}", cfg.listen))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let st: Shared = Arc::new(AppState::new(cfg, store, acl, claim.clone(), challenge_key));
    tokio::spawn(acl::follow(st.clone()));
    tokio::spawn(sweeper(st.clone()));
    let app = router(st.clone());
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "server stopped");
        }
    });
    tracing::info!(%addr, "zen-serve listening");
    Ok(Server {
        addr,
        state: st,
        claim_token: claim,
        task,
    })
}
