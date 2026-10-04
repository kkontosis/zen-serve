//! Plaintext static files from `/unencrypted` (DESIGN-3 §4.2, G15).

use crate::state::Shared;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use include_dir::{Dir, include_dir};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Component, Path, PathBuf};

static BUNDLED: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/unencrypted");

/// Load `rel` (already validated) from the configured dir or the bundle.
async fn load(st: &Shared, rel: &str) -> Option<(Vec<u8>, String)> {
    match &st.cfg.unencrypted_dir {
        None => {
            let f = BUNDLED.get_file(rel)?;
            let mut h = DefaultHasher::new();
            f.contents().hash(&mut h);
            Some((f.contents().to_vec(), format!("\"b{:x}\"", h.finish())))
        }
        Some(dir) => {
            let root = tokio::fs::canonicalize(dir).await.ok()?;
            let path = tokio::fs::canonicalize(root.join(rel)).await.ok()?;
            // Refuse symlinks that escape the directory.
            if !path.starts_with(&root) {
                return None;
            }
            let meta = tokio::fs::metadata(&path).await.ok()?;
            if !meta.is_file() {
                return None;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let body = tokio::fs::read(&path).await.ok()?;
            Some((body, format!("\"{:x}-{:x}\"", meta.len(), mtime)))
        }
    }
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

async fn serve(st: &Shared, rel: &str, req_headers: &HeaderMap, head: bool) -> Option<Response> {
    let rel = clean(rel)?;
    let (body, etag) = load(st, &rel).await?;
    let mime = mime_guess::from_path(&rel).first_or_octet_stream();
    let mut resp = if req_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        StatusCode::NOT_MODIFIED.into_response()
    } else if head {
        let mut r = Response::new(Body::empty());
        r.headers_mut()
            .insert(header::CONTENT_LENGTH, body.len().into());
        r
    } else {
        Response::new(Body::from(body))
    };
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(mime.essence_str()) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
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
    if rel == "sw.js" {
        h.insert("service-worker-allowed", HeaderValue::from_static("/"));
    }
    Some(resp)
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

/// Every request that no API route matched: `/unencrypted/*`, root aliases,
/// the SPA fallback, or 404.
pub async fn fallback(State(st): State<Shared>, req: Request) -> Response {
    let head = req.method() == Method::HEAD;
    if req.method() != Method::GET && !head {
        return (StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n").into_response();
    }
    let path = req.uri().path().to_owned();
    let headers = req.headers();
    if path.starts_with("/v1/") || path == "/v1" {
        return crate::error::not_found("no such endpoint").into_response();
    }
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
    use super::clean;

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
}
