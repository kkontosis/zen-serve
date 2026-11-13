//! Static files (api.md §10): single ranges on every source, streaming,
//! the replicated source's generations (keyspace.md §3.11), and the content
//! origin's routes (spec/sw.md §1).

mod common;

use common::*;
use std::path::Path;
use zen_proto::*;
use zen_server::config::UnencryptedSource;
use zen_server::fs0::{self, PushOptions};
use zen_server::keys;
use zen_server::statics::SANDBOX_CSP;
use zen_store::Storage;

/// Distinct bytes, so a misplaced slice shows.
fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 13) as u8 ^ seed)
        .collect()
}

async fn get(h: &Harness, path: &str, headers: &[(&str, &str)]) -> reqwest::Response {
    let mut rb = h.http.get(format!("{}{path}", h.base));
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    rb.send().await.unwrap()
}

async fn head(h: &Harness, path: &str, headers: &[(&str, &str)]) -> reqwest::Response {
    let mut rb = h.http.head(format!("{}{path}", h.base));
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    rb.send().await.unwrap()
}

fn hdr<'a>(r: &'a reqwest::Response, name: &str) -> &'a str {
    r.headers()
        .get(name)
        .unwrap_or_else(|| panic!("no {name}"))
        .to_str()
        .unwrap()
}

/// Every range form of api.md §10 / sw.md §6 against `path`, whose bytes
/// are `want` (at least 10 of them).
async fn check_ranges(h: &Harness, path: &str, want: &[u8]) {
    let len = want.len() as u64;
    assert!(len >= 10);
    let r = get(h, path, &[]).await;
    assert_eq!(r.status(), 200, "{path}");
    assert_eq!(hdr(&r, "accept-ranges"), "bytes");
    assert_eq!(hdr(&r, "content-length"), len.to_string());
    let etag = hdr(&r, "etag").to_owned();
    assert_eq!(&r.bytes().await.unwrap()[..], want, "{path}: whole");

    let parts = [
        ("bytes=0-9".to_owned(), 0, 9),
        ("bytes=5-".to_owned(), 5, len - 1),
        ("bytes=-7".to_owned(), len - 7, len - 1),
        ("bytes=3-999999999999".to_owned(), 3, len - 1),
        ("bytes=-999999999999".to_owned(), 0, len - 1),
        ("bytes=0-0".to_owned(), 0, 0),
        (format!("bytes={}-{}", len - 1, len - 1), len - 1, len - 1),
        (format!("bytes={}-", len / 2), len / 2, len - 1),
    ];
    for (range, a, b) in parts {
        let r = get(h, path, &[("range", &range)]).await;
        assert_eq!(r.status(), 206, "{path} {range}");
        assert_eq!(hdr(&r, "content-range"), format!("bytes {a}-{b}/{len}"));
        assert_eq!(hdr(&r, "content-length"), (b - a + 1).to_string());
        assert_eq!(hdr(&r, "accept-ranges"), "bytes");
        assert_eq!(hdr(&r, "etag"), etag);
        let body = r.bytes().await.unwrap();
        assert_eq!(&body[..], &want[a as usize..=b as usize], "{path} {range}");
    }
    for range in [format!("bytes={len}-"), format!("bytes={len}-{}", len + 5)] {
        let r = get(h, path, &[("range", &range)]).await;
        assert_eq!(r.status(), 416, "{path} {range}");
        assert_eq!(hdr(&r, "content-range"), format!("bytes */{len}"));
    }
    for range in ["bytes=0-1,4-5", "bytes=x", "items=0-1", "bytes=5-1"] {
        let r = get(h, path, &[("range", range)]).await;
        assert_eq!(r.status(), 200, "{path} {range}");
        assert_eq!(&r.bytes().await.unwrap()[..], want);
    }
    // If-Range: the range applies only when it names this version.
    let r = get(h, path, &[("range", "bytes=0-9"), ("if-range", &etag)]).await;
    assert_eq!(r.status(), 206);
    for other in ["\"other\"", "Wed, 21 Oct 2015 07:28:00 GMT"] {
        let r = get(h, path, &[("range", "bytes=0-9"), ("if-range", other)]).await;
        assert_eq!(r.status(), 200, "if-range {other}");
        assert_eq!(&r.bytes().await.unwrap()[..], want);
    }
    // HEAD: the same headers, no body.
    let r = head(h, path, &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, "content-length"), len.to_string());
    assert_eq!(hdr(&r, "accept-ranges"), "bytes");
    assert!(r.bytes().await.unwrap().is_empty());
    let r = head(h, path, &[("range", "bytes=0-9")]).await;
    assert_eq!(r.status(), 206);
    assert_eq!(hdr(&r, "content-range"), format!("bytes 0-9/{len}"));
    assert_eq!(hdr(&r, "content-length"), "10");
    assert!(r.bytes().await.unwrap().is_empty());
    // Validators.
    for m in [etag.as_str(), "*", &format!("\"x\", {etag}")] {
        let r = get(h, path, &[("if-none-match", m), ("range", "bytes=0-9")]).await;
        assert_eq!(r.status(), 304, "if-none-match {m}");
        assert_eq!(hdr(&r, "etag"), etag);
    }
}

/// An empty file: 200 with nothing, and every range unsatisfiable.
async fn check_empty(h: &Harness, path: &str) {
    let r = get(h, path, &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, "content-length"), "0");
    assert!(r.bytes().await.unwrap().is_empty());
    for range in ["bytes=0-", "bytes=-5", "bytes=0-0"] {
        let r = get(h, path, &[("range", range)]).await;
        assert_eq!(r.status(), 416, "{range}");
        assert_eq!(hdr(&r, "content-range"), "bytes */0");
    }
}

/// A site: a 200 KB file, an empty one, a 5 MiB one and an index.
fn site(dir: &Path) {
    std::fs::create_dir_all(dir.join("media")).unwrap();
    std::fs::write(dir.join("a.bin"), pattern(200_000, 1)).unwrap();
    std::fs::write(dir.join("empty.bin"), b"").unwrap();
    std::fs::write(dir.join("media/big.bin"), pattern(5 << 20, 2)).unwrap();
    std::fs::write(dir.join("index.html"), "<p>site</p>").unwrap();
}

/// The 5 MiB file, whole and by a range that crosses many chunks.
async fn check_big(h: &Harness) {
    let want = pattern(5 << 20, 2);
    let r = get(h, "/unencrypted/media/big.bin", &[]).await;
    assert_eq!(r.status(), 200);
    assert!(r.bytes().await.unwrap() == want, "big: whole");
    let r = get(
        h,
        "/unencrypted/media/big.bin",
        &[("range", "bytes=1000000-4500000")],
    )
    .await;
    assert_eq!(r.status(), 206);
    assert!(
        r.bytes().await.unwrap() == want[1_000_000..=4_500_000],
        "big: range"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ranges_on_the_bundled_files() {
    let h = Harness::start().await;
    let index = get(&h, "/", &[]).await.bytes().await.unwrap();
    check_ranges(&h, "/", &index).await;
    check_ranges(&h, "/unencrypted/index.html", &index).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ranges_on_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    site(dir.path());
    let www = dir.path().to_owned();
    let h = Harness::start_with(|c| c.unencrypted_dir = Some(www)).await;
    check_ranges(&h, "/unencrypted/a.bin", &pattern(200_000, 1)).await;
    check_empty(&h, "/unencrypted/empty.bin").await;
    check_big(&h).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ranges_on_the_replicated_files() {
    let dir = tempfile::tempdir().unwrap();
    site(dir.path());
    let h = Harness::start_with(|c| c.unencrypted_source = UnencryptedSource::Replicated).await;
    let p = fs0::push(
        h.server.state.store.as_ref(),
        dir.path(),
        PushOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!((p.generation, p.files), (1, 4));
    assert_eq!(p.bytes, 200_000 + (5 << 20) + 11);
    let r = get(&h, "/unencrypted/a.bin", &[]).await;
    assert!(hdr(&r, "etag").starts_with("\"s"));
    check_ranges(&h, "/unencrypted/a.bin", &pattern(200_000, 1)).await;
    check_empty(&h, "/unencrypted/empty.bin").await;
    check_big(&h).await;
}

/// The keys of generation `g` (files and chunks).
async fn keys_of(store: &dyn Storage, g: u64) -> usize {
    let mut n = 0;
    let mut t = store.begin(None).await.unwrap();
    for tag in ["sf", "sc"] {
        let p = keys::static_tag(tag).int(g as i64).finish();
        n += t
            .snapshot_get_range(&p, &keys::end_of(&p), 100_000, false)
            .await
            .unwrap()
            .len();
    }
    n
}

async fn text(h: &Harness, path: &str, headers: &[(&str, &str)]) -> (u16, String) {
    let r = get(h, path, headers).await;
    (r.status().as_u16(), r.text().await.unwrap())
}

fn write_site(dir: &Path, files: &[(&str, &str)]) {
    for (p, body) in files {
        let p = dir.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pushes_switch_generations_whole() {
    let h = Harness::start_with(|c| c.unencrypted_source = UnencryptedSource::Replicated).await;
    let store = h.server.state.store.clone();
    let push = |dir: std::path::PathBuf, opts: PushOptions| {
        let store = store.clone();
        async move { fs0::push(store.as_ref(), &dir, opts).await }
    };
    // Nothing is served before the first push.
    assert_eq!(text(&h, "/", &[]).await.0, 404);
    assert!(
        fs0::list(store.as_ref())
            .await
            .unwrap()
            .generation
            .is_none()
    );

    let a = tempfile::tempdir().unwrap();
    write_site(
        a.path(),
        &[
            ("index.html", "A"),
            ("a.txt", "only in A"),
            ("favicon.ico", "icon"),
            ("deep/er/x.css", "x"),
        ],
    );
    let p = push(a.path().to_owned(), PushOptions::default())
        .await
        .unwrap();
    assert_eq!((p.generation, p.files, p.bytes), (1, 4, 1 + 9 + 4 + 1));
    assert_eq!(text(&h, "/", &[]).await, (200, "A".into()));
    assert_eq!(text(&h, "/favicon.ico", &[]).await, (200, "icon".into()));
    assert_eq!(text(&h, "/unencrypted/deep/er/x.css", &[]).await.0, 200);
    assert_eq!(
        text(&h, "/some/route", &[("accept", "text/html")]).await,
        (200, "A".into()),
        "the SPA fallback"
    );
    let r = get(&h, "/unencrypted/deep/er/x.css", &[]).await;
    assert_eq!(hdr(&r, "content-type"), "text/css");
    let range = get(&h, "/unencrypted/a.txt", &[("range", "bytes=5-6")]).await;
    assert_eq!(range.status(), 206);
    assert_eq!(range.text().await.unwrap(), "in");

    let b = tempfile::tempdir().unwrap();
    write_site(b.path(), &[("index.html", "B"), ("b.txt", "only in B")]);
    assert_eq!(
        push(b.path().to_owned(), PushOptions::default())
            .await
            .unwrap()
            .generation,
        2
    );
    assert_eq!(text(&h, "/", &[]).await, (200, "B".into()));
    assert_eq!(text(&h, "/unencrypted/a.txt", &[]).await.0, 404);
    assert_eq!(text(&h, "/unencrypted/b.txt", &[]).await.0, 200);
    assert!(
        keys_of(store.as_ref(), 1).await > 0,
        "the previous set is kept"
    );

    let c = tempfile::tempdir().unwrap();
    write_site(c.path(), &[("index.html", "C")]);
    push(c.path().to_owned(), PushOptions::default())
        .await
        .unwrap();
    assert_eq!(keys_of(store.as_ref(), 1).await, 0, "two back: cleared");
    assert!(keys_of(store.as_ref(), 2).await > 0, "one back: kept");
    assert_eq!(text(&h, "/", &[]).await, (200, "C".into()));

    // An interrupted push serves nothing of its own, and the next push
    // skips its generation and clears it.
    let d = tempfile::tempdir().unwrap();
    write_site(d.path(), &[("index.html", "D")]);
    let p = push(
        d.path().to_owned(),
        PushOptions {
            stop_before_switch: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(p.generation, 4);
    assert_eq!(text(&h, "/", &[]).await, (200, "C".into()));
    assert_eq!(fs0::current(store.as_ref()).await.unwrap(), Some(3));
    let e = tempfile::tempdir().unwrap();
    write_site(e.path(), &[("index.html", "E"), ("e/f.txt", "f")]);
    #[cfg(unix)]
    std::os::unix::fs::symlink(e.path().join("index.html"), e.path().join("link.html")).unwrap();
    let p = push(e.path().to_owned(), PushOptions::default())
        .await
        .unwrap();
    assert_eq!(p.generation, 5);
    #[cfg(unix)]
    assert_eq!(p.skipped, vec!["link.html".to_string()]);
    assert_eq!(text(&h, "/", &[]).await, (200, "E".into()));
    assert_eq!(text(&h, "/unencrypted/link.html", &[]).await.0, 404);
    assert_eq!(
        keys_of(store.as_ref(), 4).await,
        0,
        "the leftover is cleared"
    );
    assert!(keys_of(store.as_ref(), 3).await > 0);
    assert_eq!(keys_of(store.as_ref(), 2).await, 0);

    let l = fs0::list(store.as_ref()).await.unwrap();
    assert_eq!(l.generation, Some(5));
    let names: Vec<_> = l.files.iter().map(|f| (f.0.as_str(), f.1)).collect();
    assert_eq!(names, [("e/f.txt", 1), ("index.html", 1)]);
    use sha2::Digest;
    assert_eq!(l.files[1].2, <[u8; 32]>::from(sha2::Sha256::digest(b"E")));

    // A directory with no regular file is refused, and changes nothing.
    let empty = tempfile::tempdir().unwrap();
    std::fs::create_dir(empty.path().join("sub")).unwrap();
    let err = push(empty.path().to_owned(), PushOptions::default())
        .await
        .unwrap_err();
    assert!(err.contains("no regular file"), "{err}");
    assert_eq!(fs0::current(store.as_ref()).await.unwrap(), Some(5));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cleared_generation_ends_its_bodies() {
    let h = Harness::start_with(|c| c.unencrypted_source = UnencryptedSource::Replicated).await;
    let store = h.server.state.store.clone();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.bin"), pattern(3 << 20, 3)).unwrap();
    let p = fs0::push(store.as_ref(), dir.path(), PushOptions::default())
        .await
        .unwrap();
    // Chunks from 20 on vanish, as when two later pushes clear the
    // generation while a response streams it.
    let chunks = keys::static_chunks(p.generation, "big.bin");
    let mut t = store.begin(None).await.unwrap();
    t.clear_range(
        &chunks.clone().int(20).finish(),
        &keys::end_of(&chunks.finish()),
    );
    t.commit().await.unwrap();
    let r = get(&h, "/unencrypted/big.bin", &[]).await;
    assert_eq!(r.status(), 200);
    assert!(r.bytes().await.is_err(), "the body errors, never short");
    // A range before the gap is whole.
    let r = get(&h, "/unencrypted/big.bin", &[("range", "bytes=0-99")]).await;
    assert_eq!(&r.bytes().await.unwrap()[..], &pattern(3 << 20, 3)[..100]);
}

#[tokio::test(flavor = "multi_thread")]
async fn export_and_import_keep_the_set() {
    let h = Harness::start_with(|c| c.unencrypted_source = UnencryptedSource::Replicated).await;
    let dir = tempfile::tempdir().unwrap();
    write_site(dir.path(), &[("index.html", "kept"), ("x/y.txt", "y")]);
    fs0::push(
        h.server.state.store.as_ref(),
        dir.path(),
        PushOptions::default(),
    )
    .await
    .unwrap();
    let file = h.dir.path().join("dump.zen");
    zen_server::dump::export(
        h.server.state.store.as_ref(),
        "test",
        std::fs::File::create(&file).unwrap(),
    )
    .await
    .unwrap();
    let target =
        Harness::start_with(|c| c.unencrypted_source = UnencryptedSource::Replicated).await;
    zen_server::dump::import(
        target.server.state.store.as_ref(),
        std::fs::File::open(&file).unwrap(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(text(&target, "/", &[]).await, (200, "kept".into()));
    assert_eq!(
        text(&target, "/unencrypted/x/y.txt", &[]).await,
        (200, "y".into())
    );
    let (a, b) = (
        fs0::list(h.server.state.store.as_ref()).await.unwrap(),
        fs0::list(target.server.state.store.as_ref()).await.unwrap(),
    );
    assert_eq!((a.generation, a.files), (b.generation, b.files));
}

const VIEW: &str = "http://view.test";
const APP: &str = "http://app.test";

/// A server with a content origin, and its `/_zen/` directory.
async fn content_server() -> (Harness, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    write_site(
        dir.path(),
        &[
            ("sw.js", "// the worker"),
            ("boot.html", "<p>boot</p>"),
            ("enroll.html", "<p>enroll</p>"),
            ("wasm/zen.wasm", "wasm"),
        ],
    );
    let zen = dir.path().to_owned();
    let h = Harness::start_with(|c| {
        c.public_origins = vec![APP.into()];
        c.content = Some(zen_server::config::ContentConfig {
            origins: vec![VIEW.into()],
            dir: zen,
        });
    })
    .await;
    (h, dir)
}

/// A request on the content origin.
async fn on_view(h: &Harness, path: &str, headers: &[(&str, &str)]) -> reqwest::Response {
    let mut all = vec![("host", "view.test")];
    all.extend_from_slice(headers);
    get(h, path, &all).await
}

fn assert_sandboxed(r: &reqwest::Response) {
    assert_eq!(r.status(), 503);
    assert_eq!(hdr(r, "zen-reason"), "starting");
    assert_eq!(hdr(r, "content-security-policy"), SANDBOX_CSP);
    assert_eq!(hdr(r, "x-content-type-options"), "nosniff");
    assert_eq!(hdr(r, "referrer-policy"), "no-referrer");
    assert_eq!(hdr(r, "cross-origin-resource-policy"), "same-origin");
    assert_eq!(hdr(r, "cache-control"), "no-store");
    assert_eq!(hdr(r, "content-type"), "text/html; charset=utf-8");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_content_origin_routes() {
    let (h, _dir) = content_server().await;
    let csp = zen_server::config::default_csp();

    let r = on_view(&h, "/_zen/sw.js", &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, "service-worker-allowed"), "/");
    assert_eq!(hdr(&r, "content-security-policy"), csp);
    assert_eq!(hdr(&r, "accept-ranges"), "bytes");
    assert_eq!(r.text().await.unwrap(), "// the worker");
    let r = on_view(&h, "/_zen/wasm/zen.wasm", &[("range", "bytes=1-2")]).await;
    assert_eq!(r.status(), 206);
    assert_eq!(r.text().await.unwrap(), "as");
    let r = on_view(&h, "/_zen/enroll.html", &[]).await;
    assert!(r.headers().get("service-worker-allowed").is_none());
    assert_eq!(r.text().await.unwrap(), "<p>enroll</p>");

    let r = on_view(&h, "/_zen/config.json", &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, "content-type"), "application/json");
    assert_eq!(hdr(&r, "cache-control"), "no-store");
    let v: serde_json::Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
    assert_eq!(v, serde_json::json!({ "app_origins": [APP] }));

    for missing in [
        "/_zen/nope.js",
        "/_zen/a%00b",
        "/_zen/%zz",
        "/_zen/",
        "/_zen",
    ] {
        assert_eq!(on_view(&h, missing, &[]).await.status(), 404, "{missing}");
    }
    let r = h
        .http
        .post(format!("{}/_zen/sw.js", h.base))
        .header("host", "view.test")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 405);
    assert_eq!(hdr(&r, "allow"), "GET, HEAD");

    // Navigations before the worker: the bootstrap page.
    for nav in [
        vec![("sec-fetch-mode", "navigate")],
        vec![("accept", "text/html,*/*")],
    ] {
        let r = on_view(&h, "/site/page.html", &nav).await;
        assert_eq!(r.status(), 200, "{nav:?}");
        assert_eq!(hdr(&r, "cache-control"), "no-store");
        assert_eq!(hdr(&r, "content-security-policy"), csp);
        assert_eq!(r.text().await.unwrap(), "<p>boot</p>");
    }
    // Anything else: 503, sandboxed, and nothing from /unencrypted.
    for (path, hs) in [
        ("/site/style.css", vec![("sec-fetch-mode", "no-cors")]),
        (
            "/site/page.html",
            vec![("sec-fetch-mode", "cors"), ("accept", "text/html")],
        ),
        ("/unencrypted/style.css", vec![]),
        ("/favicon.ico", vec![]),
        ("/", vec![("accept", "image/*")]),
    ] {
        let r = on_view(&h, path, &hs).await;
        assert_sandboxed(&r);
    }
    let r = h
        .http
        .post(format!("{}/site/x", h.base))
        .header("host", "view.test")
        .send()
        .await
        .unwrap();
    assert_sandboxed(&r);
    // The default port may be named.
    let r = get(&h, "/_zen/sw.js", &[("host", "view.test:80")]).await;
    assert_eq!(r.status(), 200);

    // The API answers there too, and names the content origin.
    let r = on_view(&h, "/v1/info", &[]).await;
    let info: Info = from_cbor(&r.bytes().await.unwrap()).unwrap();
    assert_eq!(info.content_origins, vec![VIEW.to_string()]);
    assert_eq!(on_view(&h, "/v1/nope", &[]).await.status(), 404);

    // The app's host is unchanged.
    let r = get(&h, "/", &[]).await;
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("<"));
    assert_eq!(get(&h, "/_zen/sw.js", &[]).await.status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_content_origin_signs_in() {
    let (h, _dir) = content_server().await;
    let (admin, bob) = (User::new(1), User::new(2));
    h.claim(&admin, &[&bob]).await;
    h.sign_in_at(&bob, VIEW, "view.test").await.unwrap();
    h.sign_in_at(&bob, APP, "app.test").await.unwrap();
    let e = h
        .sign_in_at(&bob, "http://other.test", "other.test")
        .await
        .unwrap_err();
    assert_eq!(e.0, 401);
    // /v1/info lists it among the accepted origins.
    let info: Info = h.get("/v1/info").await;
    let origins = info.auth.unwrap().origins.unwrap().origins;
    assert_eq!(origins, vec![APP.to_string(), VIEW.to_string()]);
}
