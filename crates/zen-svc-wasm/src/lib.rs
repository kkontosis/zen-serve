//! WASM bindings for `@zen/svc` (docs/MILESTONE-5.5.md, step 8): the TLS
//! engine of zen-svc (svc.md §7.1), as a second module that only services
//! using `tls()` load.
//!
//! * [`TlsConfig`]: a certificate chain and its key, and the TLS options.
//! * [`TlsConn`]: one connection. During the handshake it lives in memory
//!   only; once established, [`TlsConn::state`] serializes it and
//!   [`TlsConn::restore`] continues it, in this handler or another.
//!
//! Handles hold keys in WASM memory and zeroize them on `free()`. Errors
//! are JS `Error`s named `ZenTlsError` whose message is a stable code:
//! `config`, `state`, `handshaking`, `closed`, `key_limit`, `unsupported`,
//! `tls`, `alert_sent`, `alert_received` or `internal`, with the details in
//! `detail`.

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{CipherSuite, ProtocolVersion};
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use zen_svc::tls::{self, Error};

#[wasm_bindgen(typescript_custom_section)]
const TS_TYPES: &str = r#"
/** Options of a `TlsConfig`. */
export interface TlsOptions {
  /** ALPN protocols, preferred first. */
  alpn?: Uint8Array[];
  /** Offer TLS 1.2 (default true). */
  tls12?: boolean;
  /** Offer TLS 1.3 (default true). */
  tls13?: boolean;
  /** Tests only: records per key before the limit's action (svc.md §7.1). */
  keyLimit?: number;
}

/** Why a connection failed, or what a call refused. */
export interface TlsFailure {
  code: 'config' | 'state' | 'handshaking' | 'closed' | 'key_limit' | 'unsupported'
    | 'tls' | 'alert_sent' | 'alert_received' | 'internal';
  /** The alert sent or received, e.g. `BadRecordMac`. */
  alert?: string;
  detail: string;
}

/** What one `TlsConn.input` did. */
export interface TlsProgress {
  /** Input bytes processed: the step's ack (a record boundary once established). */
  consumed: number;
  /** Bytes for the peer: handshake flights, alerts, KeyUpdates. */
  out: Uint8Array;
  /** This call finished the handshake. */
  established: boolean;
  /** The peer sent close_notify. */
  peerClosed: boolean;
  /** The input ended without close_notify. */
  truncated: boolean;
  /** The plaintext held reached `plainHeld()` with input left: consume, then input again. */
  held: boolean;
  /** The connection failed; any alert for the peer is in `out`. */
  error?: TlsFailure;
}
"#;

fn code(e: &Error) -> &'static str {
    match e {
        Error::Config(_) => "config",
        Error::BadState(_) => "state",
        Error::Handshaking => "handshaking",
        Error::Closed => "closed",
        Error::KeyLimit => "key_limit",
        Error::Unsupported(_) => "unsupported",
        Error::Tls(_) => "tls",
        Error::AlertSent(_) => "alert_sent",
        Error::AlertReceived(_) => "alert_received",
        Error::Internal(_) => "internal",
    }
}

fn set(o: &js_sys::Object, k: &str, v: impl Into<JsValue>) {
    // Setting a property on a plain object can't fail.
    let _ = js_sys::Reflect::set(o, &JsValue::from_str(k), &v.into());
}

fn failure(e: &Error) -> js_sys::Object {
    let o = js_sys::Object::new();
    set(&o, "code", code(e));
    if let Error::AlertSent(a) | Error::AlertReceived(a) = e {
        set(&o, "alert", format!("{a:?}"));
    }
    set(&o, "detail", e.to_string());
    o
}

/// A `ZenTlsError` with a stable code as its message.
fn js_err(e: &Error) -> JsValue {
    let err = js_sys::Error::new(code(e));
    err.set_name("ZenTlsError");
    set(&err, "detail", e.to_string());
    err.into()
}

fn config_err(detail: impl Into<String>) -> JsValue {
    js_err(&Error::Config(detail.into()))
}

/// The plaintext an established connection holds before it stops reading
/// records (svc.md §7.1).
#[wasm_bindgen(js_name = plainHeld)]
pub fn plain_held() -> u32 {
    tls::PLAIN_HELD as u32
}

/// A server certificate and the TLS settings, shared by connections.
#[wasm_bindgen]
pub struct TlsConfig(Arc<tls::Config>);

#[wasm_bindgen]
impl TlsConfig {
    /// `certPem`: the chain, leaf first. `keyPem`: its key, PKCS#8 or SEC1
    /// (ECDSA P-256 or P-384, or Ed25519; RSA is refused). Throws `config`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        cert_pem: &[u8],
        key_pem: &[u8],
        #[wasm_bindgen(unchecked_optional_param_type = "TlsOptions")] opts: Option<js_sys::Object>,
    ) -> Result<TlsConfig, JsValue> {
        let chain = CertificateDer::pem_slice_iter(cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| config_err(format!("certificate PEM: {e:?}")))?;
        let key = PrivateKeyDer::from_pem_slice(key_pem)
            .map_err(|e| config_err(format!("key PEM: {e:?}")))?;
        let o = options(opts.map(JsValue::from).unwrap_or(JsValue::UNDEFINED))?;
        tls::Config::new(chain, key, o)
            .map(|c| TlsConfig(Arc::new(c)))
            .map_err(|e| js_err(&e))
    }
}

fn options(v: JsValue) -> Result<tls::Options, JsValue> {
    let mut o = tls::Options::default();
    if v.is_undefined() || v.is_null() {
        return Ok(o);
    }
    let get =
        |k: &str| js_sys::Reflect::get(&v, &JsValue::from_str(k)).unwrap_or(JsValue::UNDEFINED);
    let flag = |k: &str, d: bool| -> Result<bool, JsValue> {
        let x = get(k);
        if x.is_undefined() {
            return Ok(d);
        }
        x.as_bool()
            .ok_or_else(|| config_err(format!("{k}: not a boolean")))
    };
    o.tls12 = flag("tls12", true)?;
    o.tls13 = flag("tls13", true)?;
    let alpn = get("alpn");
    if !alpn.is_undefined() {
        let arr = alpn
            .dyn_into::<js_sys::Array>()
            .map_err(|_| config_err("alpn: not an array"))?;
        for p in arr.iter() {
            let p = p
                .dyn_into::<js_sys::Uint8Array>()
                .map_err(|_| config_err("alpn: not a Uint8Array"))?;
            if p.length() == 0 || p.length() > 255 {
                return Err(config_err("alpn: a protocol of 1 to 255 bytes"));
            }
            o.alpn.push(p.to_vec());
        }
    }
    let limit = get("keyLimit");
    if !limit.is_undefined() {
        let n = limit
            .as_f64()
            .filter(|n| n.fract() == 0.0 && *n >= 1.0 && *n <= 9e15)
            .ok_or_else(|| config_err("keyLimit: a positive integer"))?;
        o.key_limit = Some(n as u64);
    }
    Ok(o)
}

/// One TLS connection, server side.
#[wasm_bindgen]
pub struct TlsConn(tls::Conn);

#[wasm_bindgen]
impl TlsConn {
    /// A new connection, before the client's first byte.
    pub fn start(config: &TlsConfig) -> Result<TlsConn, JsValue> {
        tls::Conn::new(&config.0)
            .map(TlsConn)
            .map_err(|e| js_err(&e))
    }

    /// An established connection from its `state()`. Throws `state`.
    pub fn restore(config: &TlsConfig, state: &[u8]) -> Result<TlsConn, JsValue> {
        tls::Conn::restore(&config.0, state)
            .map(TlsConn)
            .map_err(|e| js_err(&e))
    }

    /// Processes input from the peer. `buf` starts at the first byte not
    /// yet consumed; `eof`: the peer's input ends after it.
    #[wasm_bindgen(unchecked_return_type = "TlsProgress")]
    pub fn input(&mut self, buf: &[u8], eof: bool) -> JsValue {
        let p = self.0.input(buf, eof);
        let held = p.error.is_none()
            && p.consumed < buf.len()
            && self.0.plaintext().len() >= tls::PLAIN_HELD;
        let o = js_sys::Object::new();
        set(&o, "consumed", p.consumed as u32);
        set(&o, "out", js_sys::Uint8Array::from(&p.out[..]));
        set(&o, "established", p.established);
        set(&o, "peerClosed", p.peer_closed);
        set(&o, "truncated", p.truncated);
        set(&o, "held", held);
        if let Some(e) = &p.error {
            set(&o, "error", failure(e));
        }
        o.into()
    }

    /// Decrypted application data not consumed yet.
    pub fn plaintext(&self) -> Vec<u8> {
        self.0.plaintext().to_vec()
    }

    /// Consumes the first `n` bytes of the plaintext.
    pub fn consume(&mut self, n: u32) {
        self.0.consume(n as usize);
    }

    /// Encrypts application data into records. Throws `handshaking`,
    /// `closed`, or `key_limit` (TLS 1.2: close the connection).
    pub fn write(&mut self, data: &[u8]) -> Result<Vec<u8>, JsValue> {
        self.0.write(data).map_err(|e| js_err(&e))
    }

    /// A KeyUpdate (TLS 1.3), asking the peer to update too if
    /// `requestPeer`. Throws `unsupported` in TLS 1.2.
    #[wasm_bindgen(js_name = keyUpdate)]
    pub fn key_update(&mut self, request_peer: bool) -> Result<Vec<u8>, JsValue> {
        self.0.key_update(request_peer).map_err(|e| js_err(&e))
    }

    /// close_notify, once established (empty during the handshake).
    pub fn close(&mut self) -> Vec<u8> {
        self.0.close()
    }

    /// The state to save with the step; undefined until the handshake is
    /// done, and after it failed.
    pub fn state(&self) -> Option<Vec<u8>> {
        self.0.state()
    }

    /// Whether the handshake is done.
    #[wasm_bindgen(getter)]
    pub fn established(&self) -> bool {
        self.0.is_established()
    }

    /// The peer sent close_notify.
    #[wasm_bindgen(getter, js_name = peerClosed)]
    pub fn peer_closed(&self) -> bool {
        self.0.session().is_some_and(|s| s.peer_closed())
    }

    /// This side can't send application data any more.
    #[wasm_bindgen(getter, js_name = writeClosed)]
    pub fn write_closed(&self) -> bool {
        self.0.session().is_none_or(|s| s.write_closed())
    }

    /// `TLSv1.3` or `TLSv1.2`, once established.
    #[wasm_bindgen(getter)]
    pub fn version(&self) -> Option<String> {
        self.0.session().map(|s| {
            match s.version() {
                ProtocolVersion::TLSv1_3 => "TLSv1.3",
                _ => "TLSv1.2",
            }
            .to_string()
        })
    }

    /// The cipher suite's IANA name, once established.
    #[wasm_bindgen(getter)]
    pub fn suite(&self) -> Option<String> {
        self.0.session().map(|s| suite_name(s.suite()))
    }

    /// The client's server name, if it sent one.
    #[wasm_bindgen(getter)]
    pub fn sni(&self) -> Option<String> {
        self.0.session().and_then(|s| s.sni()).map(str::to_owned)
    }

    /// The negotiated application protocol, if any.
    #[wasm_bindgen(getter)]
    pub fn alpn(&self) -> Option<Vec<u8>> {
        self.0.session().and_then(|s| s.alpn()).map(<[u8]>::to_vec)
    }
}

/// rustls names the TLS 1.3 suites `TLS13_…`; IANA's names are `TLS_…`.
fn suite_name(s: CipherSuite) -> String {
    match s.as_str() {
        Some(n) => n
            .strip_prefix("TLS13_")
            .map_or(n.to_string(), |r| format!("TLS_{r}")),
        None => format!("0x{:04x}", u16::from(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suite_names_are_iana() {
        assert_eq!(
            suite_name(CipherSuite::TLS13_AES_128_GCM_SHA256),
            "TLS_AES_128_GCM_SHA256"
        );
        assert_eq!(
            suite_name(CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256),
            "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256"
        );
    }
}
