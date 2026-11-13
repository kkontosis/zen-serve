//! Plaintext static files: `/unencrypted` (DESIGN-3 §4.2, G15, api.md §10)
//! from a directory, the bundle or the store (`fs0`), streamed, with single
//! ranges; and the content origin's routes (spec/sw.md §1).

use crate::config::UnencryptedSource;
use crate::fs0;
use crate::state::Shared;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use include_dir::{Dir, include_dir};
use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use zen_store::Storage;

static BUNDLED: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/unencrypted");

/// The `Content-Security-Policy` of every response the service worker
/// makes (spec/sw.md §4), and of the server's 503 `starting` on the
/// content origin: no script runs.
pub const SANDBOX_CSP: &str = "sandbox allow-same-origin allow-popups \
    allow-popups-to-escape-sandbox allow-downloads; default-src 'self' data: blob:; \
    script-src 'none'; object-src 'none'; style-src 'self' 'unsafe-inline'; \
    base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Bytes read from a directory file per body piece.
const PIECE: usize = 64 * 1024;

/// Where a file's bytes come from.
enum Src {
    Bundled(&'static [u8]),
    Disk(tokio::fs::File),
    Store { generation: u64, path: String },
}

/// An opened file.
struct File {
    len: u64,
    etag: String,
    src: Src,
}

/// What a `Range` header asks for (RFC 9110 §14, spec/sw.md §6).
#[derive(Debug, PartialEq, Eq)]
pub enum Range {
    /// The whole file: no range, several, or a malformed one.
    Full,
    /// Bytes `first..=last`, within the file.
    Part(u64, u64),
    /// 416.
    Unsatisfiable,
}

fn digits(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Read a `Range` header against a file of `len` bytes. Only one range of
/// bytes is honoured; anything else means the whole file.
pub fn parse_range(h: &str, len: u64) -> Range {
    let h = h.trim();
    let Some(spec) = h
        .get(..6)
        .filter(|u| u.eq_ignore_ascii_case("bytes="))
        .map(|_| &h[6..])
    else {
        return Range::Full;
    };
    if spec.contains(',') {
        return Range::Full;
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return Range::Full;
    };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        // `-n`: the last n bytes.
        let Some(n) = digits(b) else {
            return Range::Full;
        };
        if n == 0 || len == 0 {
            return Range::Unsatisfiable;
        }
        return Range::Part(len.saturating_sub(n), len - 1);
    }
    let Some(first) = digits(a) else {
        return Range::Full;
    };
    let last = if b.is_empty() {
        u64::MAX
    } else {
        match digits(b) {
            Some(l) if l >= first => l,
            _ => return Range::Full,
        }
    };
    if first >= len {
        return Range::Unsatisfiable;
    }
    Range::Part(first, last.min(len - 1))
}

/// Percent-decode a URL path; `None` if malformed or not UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Validate a request path relative to `/unencrypted`.
fn clean(rel: &str) -> Option<String> {
    let rel = percent_decode(rel)?;
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() || rel.contains('\\') || rel.contains('\0') {
        return None;
    }
    let p = Path::new(rel);
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => out.push(s),
            _ => return None,
        }
    }
    out.to_str().map(str::to_owned)
}

fn open_bundled(rel: &str) -> Option<File> {
    let f = BUNDLED.get_file(rel)?;
    let mut h = DefaultHasher::new();
    f.contents().hash(&mut h);
    Some(File {
        len: f.contents().len() as u64,
        etag: format!("\"b{:x}\"", h.finish()),
        src: Src::Bundled(f.contents()),
    })
}

/// Open `rel` (already validated) below `dir`, refusing symlinks that
/// escape it.
async fn open_dir(dir: &Path, rel: &str) -> Option<File> {
    let root = tokio::fs::canonicalize(dir).await.ok()?;
    let path = tokio::fs::canonicalize(root.join(rel)).await.ok()?;
    if !path.starts_with(&root) {
        return None;
    }
    let f = tokio::fs::File::open(&path).await.ok()?;
    let meta = f.metadata().await.ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some(File {
        len: meta.len(),
        etag: format!("\"{:x}-{:x}\"", meta.len(), mtime),
        src: Src::Disk(f),
    })
}

/// A file that can't be read now: the store is down, or a seek failed.
#[derive(Debug)]
struct Unavailable;

impl IntoResponse for Unavailable {
    fn into_response(self) -> Response {
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable\n").into_response()
    }
}

async fn open_store(store: &dyn Storage, rel: &str) -> Result<Option<File>, Unavailable> {
    let m = fs0::open(store, rel).await.map_err(|e| {
        tracing::warn!(error = %e.message, "static files: storage");
        Unavailable
    })?;
    Ok(m.map(|m| {
        let hex: String = m.sha256[..16].iter().map(|b| format!("{b:02x}")).collect();
        File {
            len: m.size,
            etag: format!("\"s{hex}\""),
            src: Src::Store {
                generation: m.generation,
                path: rel.to_owned(),
            },
        }
    }))
}

/// Open `rel` (already validated) at `/unencrypted`.
async fn open(st: &Shared, rel: &str) -> Result<Option<File>, Unavailable> {
    match (st.cfg.unencrypted_source, &st.cfg.unencrypted_dir) {
        (UnencryptedSource::Replicated, _) => open_store(st.store.as_ref(), rel).await,
        (UnencryptedSource::Dir, Some(dir)) => Ok(open_dir(dir, rel).await),
        (UnencryptedSource::Dir, None) => Ok(open_bundled(rel)),
    }
}

fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// The next `n` bytes of a directory file, streamed.
fn disk_body(f: tokio::fs::File, n: u64) -> Body {
    let s = futures::stream::unfold((Some(f), n), |(f, left)| async move {
        let mut f = f?;
        if left == 0 {
            return None;
        }
        let mut buf = vec![0u8; left.min(PIECE as u64) as usize];
        match f.read(&mut buf).await {
            Ok(0) => Some((
                Err(io_err("the file shrank while it was served")),
                (None, 0),
            )),
            Ok(k) => {
                buf.truncate(k);
                Some((Ok(Bytes::from(buf)), (Some(f), left - k as u64)))
            }
            Err(e) => Some((Err(e), (None, 0))),
        }
    });
    Body::from_stream(s)
}

/// `n` bytes from `first` of a file in the store, streamed in batches of
/// [`fs0::READ_BATCH`] chunks.
fn store_body(store: Arc<dyn Storage>, generation: u64, path: String, first: u64, n: u64) -> Body {
    struct S {
        store: Arc<dyn Storage>,
        generation: u64,
        path: String,
        next: u64,
        skip: usize,
        left: u64,
        buf: VecDeque<Vec<u8>>,
    }
    let chunk = fs0::CHUNK as u64;
    let last = (first + n).div_ceil(chunk);
    let st = S {
        store,
        generation,
        path,
        next: first / chunk,
        skip: (first % chunk) as usize,
        left: n,
        buf: VecDeque::new(),
    };
    let s = futures::stream::unfold(st, move |mut s| async move {
        if s.left == 0 {
            return None;
        }
        if s.buf.is_empty() {
            let want = fs0::READ_BATCH.min(last - s.next);
            match fs0::chunks(s.store.as_ref(), s.generation, &s.path, s.next, want).await {
                Ok(got) if got.len() as u64 == want => {
                    s.next += want;
                    s.buf.extend(got);
                }
                Ok(_) => {
                    s.left = 0;
                    return Some((Err(io_err("the file's generation was cleared")), s));
                }
                Err(e) => {
                    s.left = 0;
                    return Some((Err(io_err(e.message)), s));
                }
            }
        }
        let c = s.buf.pop_front().unwrap_or_default();
        let start = std::mem::take(&mut s.skip).min(c.len());
        let take = ((c.len() - start) as u64).min(s.left) as usize;
        if take == 0 {
            s.left = 0;
            return Some((Err(io_err("a chunk is shorter than its file says")), s));
        }
        s.left -= take as u64;
        Some((Ok(Bytes::copy_from_slice(&c[start..start + take])), s))
    });
    Body::from_stream(s)
}

/// The body of bytes `first..first + n` of `f`.
async fn body(st: &Shared, src: Src, first: u64, n: u64) -> Result<Body, Unavailable> {
    Ok(match src {
        Src::Bundled(b) => Body::from(Bytes::from_static(&b[first as usize..(first + n) as usize])),
        Src::Disk(mut f) => {
            f.seek(std::io::SeekFrom::Start(first)).await.map_err(|e| {
                tracing::warn!(error = %e, "static files: seek");
                Unavailable
            })?;
            disk_body(f, n)
        }
        Src::Store { generation, path } => store_body(st.store.clone(), generation, path, first, n),
    })
}

/// How a file is served.
#[derive(Default, Clone, Copy)]
struct Opts {
    /// `Cache-Control: no-store` rather than `no-cache`.
    no_store: bool,
    /// `Service-Worker-Allowed: /`.
    sw_allowed: bool,
}

/// The headers of the server's own pages: caching, `nosniff`, no referrer,
/// and the configured CSP.
fn trusted_headers(st: &Shared, h: &mut HeaderMap, no_store: bool) {
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if no_store { "no-store" } else { "no-cache" }),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if let Ok(v) = HeaderValue::from_str(&st.cfg.csp) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
}

/// sw.md §4's headers, on the content origin's 503 `starting`.
pub fn sandbox_headers(h: &mut HeaderMap) {
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(SANDBOX_CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

/// Answer a request for an opened file: 304, 206, 416 or 200, for GET or
/// HEAD.
async fn respond(
    st: &Shared,
    name: &str,
    f: File,
    req: &HeaderMap,
    head: bool,
    opts: Opts,
) -> Response {
    let text = |h: header::HeaderName| req.get(h).and_then(|v| v.to_str().ok());
    let not_modified = text(header::IF_NONE_MATCH)
        .is_some_and(|v| v.split(',').any(|t| t.trim() == f.etag || t.trim() == "*"));
    let range = match text(header::RANGE) {
        Some(r) if text(header::IF_RANGE).is_none_or(|ir| ir.trim() == f.etag) => {
            parse_range(r, f.len)
        }
        _ => Range::Full,
    };
    let mut resp = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else if range == Range::Unsatisfiable {
        let mut r = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        if let Ok(v) = HeaderValue::from_str(&format!("bytes */{}", f.len)) {
            r.headers_mut().insert(header::CONTENT_RANGE, v);
        }
        r
    } else {
        let (status, first, n) = match range {
            Range::Part(a, b) => (StatusCode::PARTIAL_CONTENT, a, b - a + 1),
            _ => (StatusCode::OK, 0, f.len),
        };
        let b = if head {
            Body::empty()
        } else {
            match body(st, f.src, first, n).await {
                Ok(b) => b,
                Err(r) => return r.into_response(),
            }
        };
        let mut r = Response::new(b);
        *r.status_mut() = status;
        let h = r.headers_mut();
        h.insert(header::CONTENT_LENGTH, n.into());
        if status == StatusCode::PARTIAL_CONTENT
            && let Ok(v) =
                HeaderValue::from_str(&format!("bytes {first}-{}/{}", first + n - 1, f.len))
        {
            h.insert(header::CONTENT_RANGE, v);
        }
        if let Ok(v) = HeaderValue::from_str(
            mime_guess::from_path(name)
                .first_or_octet_stream()
                .essence_str(),
        ) {
            h.insert(header::CONTENT_TYPE, v);
        }
        r
    };
    let h = resp.headers_mut();
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(v) = HeaderValue::from_str(&f.etag) {
        h.insert(header::ETAG, v);
    }
    trusted_headers(st, h, opts.no_store);
    if opts.sw_allowed {
        h.insert("service-worker-allowed", HeaderValue::from_static("/"));
    }
    resp
}

/// Serve `rel` from `/unencrypted`; `None` if there is no such file.
async fn serve(st: &Shared, rel: &str, req: &HeaderMap, head: bool) -> Option<Response> {
    let rel = clean(rel)?;
    let f = match open(st, &rel).await {
        Ok(f) => f?,
        Err(r) => return Some(r.into_response()),
    };
    let opts = Opts {
        sw_allowed: rel == "sw.js",
        ..Opts::default()
    };
    Some(respond(st, &rel, f, req, head, opts).await)
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

/// The content origin's directory, if the request's `Host` is one of the
/// content origins (its default port optional).
fn content_dir<'a>(st: &'a Shared, req: &Request) -> Option<&'a Path> {
    let c = st.cfg.content.as_ref()?;
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()))?
        .to_ascii_lowercase();
    c.origins
        .iter()
        .any(|o| {
            let (scheme, authority) = o.split_once("://").unwrap_or(("", o));
            let default = if scheme == "https" { ":443" } else { ":80" };
            host == authority || host.strip_suffix(default) == Some(authority)
        })
        .then_some(c.dir.as_path())
}

/// An API path no route matched.
fn is_api(path: &str) -> bool {
    path.starts_with("/v1/") || path == "/v1"
}

/// Whether a request is a navigation: `Sec-Fetch-Mode: navigate`, or,
/// without that header, an `Accept` naming HTML.
fn is_navigation(h: &HeaderMap) -> bool {
    match h.get("sec-fetch-mode").and_then(|v| v.to_str().ok()) {
        Some(m) => m == "navigate",
        None => h
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/html")),
    }
}

const STARTING_HTML: &str = "<!doctype html>\n<meta charset=\"utf-8\">\n<title>Starting</title>\n\
    <p>The viewer is starting. Reload the page.</p>\n";

/// 503 `Zen-Reason: starting` under sw.md §4's headers: a request on the
/// content origin that reached the server before the worker.
fn starting() -> Response {
    let mut r = (StatusCode::SERVICE_UNAVAILABLE, STARTING_HTML).into_response();
    let h = r.headers_mut();
    sandbox_headers(h);
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert("zen-reason", HeaderValue::from_static("starting"));
    r
}

/// A request on the content origin (spec/sw.md §1, api.md §10).
async fn content(
    st: &Shared,
    dir: &Path,
    path: &str,
    method: &Method,
    headers: &HeaderMap,
) -> Response {
    let head = method == Method::HEAD;
    let get = head || method == Method::GET;
    if is_api(path) {
        // As on the app's host.
        return if get {
            crate::error::not_found("no such endpoint").into_response()
        } else {
            (StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n").into_response()
        };
    }
    if let Some(rel) = path.strip_prefix("/_zen/") {
        if !get {
            let mut r = (StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n").into_response();
            r.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
            return r;
        }
        if rel == "config.json" {
            let body = serde_json::json!({ "app_origins": st.cfg.public_origins }).to_string();
            let mut r = if head {
                let mut r = Response::new(Body::empty());
                r.headers_mut()
                    .insert(header::CONTENT_LENGTH, body.len().into());
                r
            } else {
                Response::new(Body::from(body))
            };
            let h = r.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            trusted_headers(st, h, true);
            return r;
        }
        let Some(rel) = clean(rel) else {
            return not_found();
        };
        return match open_dir(dir, &rel).await {
            Some(f) => {
                let opts = Opts {
                    sw_allowed: rel == "sw.js",
                    ..Opts::default()
                };
                respond(st, &rel, f, headers, head, opts).await
            }
            None => not_found(),
        };
    }
    if path == "/_zen" {
        return not_found();
    }
    if get && is_navigation(headers) {
        if let Some(f) = open_dir(dir, "boot.html").await {
            let opts = Opts {
                no_store: true,
                ..Opts::default()
            };
            return respond(st, "boot.html", f, headers, head, opts).await;
        }
        tracing::warn!(dir = %dir.display(), "content origin: no boot.html");
    }
    starting()
}

/// Every request that no API route matched: the content origin, then
/// `/unencrypted/*`, root aliases, the SPA fallback, or 404.
pub async fn fallback(State(st): State<Shared>, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    if let Some(dir) = content_dir(&st, &req) {
        let dir = dir.to_owned();
        return content(&st, &dir, &path, req.method(), req.headers()).await;
    }
    let head = req.method() == Method::HEAD;
    if req.method() != Method::GET && !head {
        return (StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n").into_response();
    }
    if is_api(&path) {
        return crate::error::not_found("no such endpoint").into_response();
    }
    let headers = req.headers();
    let target = if let Some(rel) = path.strip_prefix("/unencrypted/") {
        Some(rel.to_owned())
    } else if path == "/" || path == "/index.html" {
        Some("index.html".to_owned())
    } else {
        let name = path.trim_start_matches('/');
        st.cfg
            .aliases
            .iter()
            .any(|a| a == name)
            .then(|| name.to_owned())
    };
    if let Some(rel) = target {
        return serve(&st, &rel, headers, head)
            .await
            .unwrap_or_else(not_found);
    }
    let wants_html = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"));
    if st.cfg.spa_fallback
        && wants_html
        && let Some(r) = serve(&st, "index.html", headers, head).await
    {
        return r;
    }
    not_found()
}

#[cfg(test)]
mod tests {
    use super::{Range, clean, parse_range};

    #[test]
    fn path_cleaning() {
        assert_eq!(clean("a/b.js").as_deref(), Some("a/b.js"));
        assert_eq!(clean("/x.css").as_deref(), Some("x.css"));
        assert_eq!(clean("../etc/passwd"), None);
        assert_eq!(clean("a/../../b"), None);
        assert_eq!(clean("a/./b"), Some("a/b".into()));
        assert_eq!(clean(""), None);
        assert_eq!(clean("a\\b"), None);
        assert_eq!(clean("%2e%2e/x"), None);
        assert_eq!(clean("my%20file.txt").as_deref(), Some("my file.txt"));
        assert_eq!(clean("%zz"), None);
    }

    #[test]
    fn ranges() {
        use Range::*;
        let p = |h| parse_range(h, 100);
        assert_eq!(p("bytes=0-9"), Part(0, 9));
        assert_eq!(p("bytes=90-"), Part(90, 99));
        assert_eq!(p("bytes=-10"), Part(90, 99));
        assert_eq!(p("bytes=-1000"), Part(0, 99));
        assert_eq!(p("bytes=50-1000"), Part(50, 99));
        assert_eq!(p("bytes=0-0"), Part(0, 0));
        assert_eq!(p("bytes=99-99"), Part(99, 99));
        assert_eq!(p("BYTES=1-2"), Part(1, 2));
        assert_eq!(p("bytes=100-"), Unsatisfiable);
        assert_eq!(p("bytes=100-200"), Unsatisfiable);
        assert_eq!(p("bytes=-0"), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=-5", 0), Unsatisfiable);
        for full in [
            "bytes=0-1,5-6",
            "bytes=x",
            "items=0-1",
            "bytes=5-1",
            "bytes=+1-2",
            "bytes=-",
            "bytes=1",
            "bytes=-+5",
            "bytes=99999999999999999999-",
            "",
        ] {
            assert_eq!(p(full), Full, "{full:?}");
        }
    }
}
