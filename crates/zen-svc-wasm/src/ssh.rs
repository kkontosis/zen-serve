//! The SSH engine of zen-svc (svc.md §7.2) for `@zen/svc`'s `ssh()` layer.
//!
//! * [`SshConfig`]: the host key and the SSH options.
//! * [`SshConn`]: one connection. Its whole state serializes at every
//!   step ([`SshConn::state`]), a key exchange in progress included, and
//!   [`SshConn::restore`] continues it in this handler or another.
//!
//! Errors are JS `Error`s named `ZenSshError` whose message is a stable
//! code: `config`, `key`, `state`, `closed`, `no_channel`,
//! `channel_ended` or `not_asked`, with the details in `detail`. A
//! connection's failure in `input` is `{code, reason?, detail}`, with the
//! codes `protocol` (the engine sent DISCONNECT with `reason`) and
//! `disconnected` (the client did).

use super::set;
use wasm_bindgen::prelude::*;
use zen_svc::ssh::{self, Cipher, Error, Event, Kex, PublicKey, Request};

#[wasm_bindgen(typescript_custom_section)]
const TS_TYPES: &str = r#"
/** Options of an `SshConfig`. */
export interface SshOptions {
  /** The window each channel is offered (default 98,304; svc.md §7.2). */
  window?: number;
  /** Bytes in either direction under one key before the server rekeys (default 1 GiB). */
  rekeyBytes?: number;
  /** Seconds since the last key exchange before the server rekeys (default 3,600). */
  rekeySecs?: number;
  /** Key-exchange methods, preferred first: `mlkem768x25519-sha256`, `curve25519-sha256`. */
  kex?: string[];
  /** Ciphers, preferred first: `chacha20-poly1305@openssh.com`, `aes256-gcm@openssh.com`. */
  ciphers?: string[];
  /** The software name in `SSH-2.0-<software>` (default `zen`). */
  software?: string;
}

/** A user's public key. */
export interface SshKey {
  /** `ssh-ed25519` or `ssh-rsa`. */
  algorithm: string;
  /** `SHA256:…`, as `ssh-keygen -l` prints it. */
  fingerprint: string;
  /** The key in its SSH encoding: equal blobs are the same key. */
  blob: Uint8Array;
}

/** May `key` log in as `user`? `signed`: the client proved it holds the key. */
export interface SshAuthorize {
  user: string;
  key: SshKey;
  signed: boolean;
}

/** What the application is told about, from one `input`. */
export type SshEngineEvent =
  | ({ type: 'authorize' } & SshAuthorize)
  | { type: 'open'; ch: number }
  | { type: 'exec'; ch: number; command: Uint8Array }
  | { type: 'shell'; ch: number }
  | { type: 'subsystem'; ch: number; name: string }
  | { type: 'data'; ch: number }
  | { type: 'eof'; ch: number }
  | { type: 'close'; ch: number };

/** Why a connection ended, or what a call refused. */
export interface SshFailure {
  code: 'config' | 'key' | 'state' | 'closed' | 'no_channel' | 'channel_ended' | 'not_asked'
    | 'protocol' | 'disconnected';
  /** The DISCONNECT reason (RFC 4253 §11.1), sent or received. */
  reason?: number;
  detail: string;
}

/** What one `SshConn.input` did. */
export interface SshProgress {
  /** Input bytes processed: the step's ack, at a packet boundary. */
  consumed: number;
  /** Bytes for the client. */
  out: Uint8Array;
  events: SshEngineEvent[];
  /** The client sent DISCONNECT, or its input ended. */
  peerClosed: boolean;
  /** Reading stopped with complete input left: answer or consume, then input again. */
  held: boolean;
  /** The connection ended; a DISCONNECT for the client is in `out`. */
  error?: SshFailure;
}

/** A channel as `SshConn.channels` shows it. */
export interface SshChannelInfo {
  ch: number;
  request: 'exec' | 'shell' | 'subsystem' | null;
  /** The client sent EOF. */
  eof: boolean;
  /** The client closed the channel. */
  closed: boolean;
  /** Data not consumed yet. */
  unread: number;
  /** Output queued. */
  queued: number;
  /** What the client's window still takes. */
  peerWindow: number;
}

/** What a write took, and the bytes to send. */
export interface SshWrite {
  taken: number;
  out: Uint8Array;
}
"#;

fn code(e: &Error) -> &'static str {
    match e {
        Error::Config(_) => "config",
        Error::Key(_) => "key",
        Error::BadState(_) => "state",
        Error::Closed => "closed",
        Error::NoChannel => "no_channel",
        Error::ChannelEnded => "channel_ended",
        Error::NotAsked => "not_asked",
        Error::Protocol { .. } => "protocol",
        Error::Disconnected { .. } => "disconnected",
    }
}

fn failure(e: &Error) -> js_sys::Object {
    let o = js_sys::Object::new();
    set(&o, "code", code(e));
    if let Error::Protocol { reason, .. } | Error::Disconnected { reason, .. } = e {
        set(&o, "reason", *reason);
    }
    set(&o, "detail", e.to_string());
    o
}

/// A `ZenSshError` with a stable code as its message.
fn js_err(e: &Error) -> JsValue {
    let err = js_sys::Error::new(code(e));
    err.set_name("ZenSshError");
    set(&err, "detail", e.to_string());
    err.into()
}

fn config_err(detail: impl Into<String>) -> JsValue {
    js_err(&Error::Config(detail.into()))
}

fn bytes(b: &[u8]) -> js_sys::Uint8Array {
    js_sys::Uint8Array::from(b)
}

fn key_value(k: &PublicKey) -> js_sys::Object {
    let o = js_sys::Object::new();
    set(&o, "algorithm", k.algorithm());
    set(&o, "fingerprint", k.fingerprint());
    set(&o, "blob", bytes(k.blob()));
    o
}

fn authorize_value(a: &ssh::Authorize, o: &js_sys::Object) {
    set(o, "user", a.user.as_str());
    set(o, "key", key_value(&a.key));
    set(o, "signed", a.signed);
}

fn event_value(e: &Event) -> js_sys::Object {
    let o = js_sys::Object::new();
    let (typ, ch) = match e {
        Event::Authorize(a) => {
            authorize_value(a, &o);
            ("authorize", None)
        }
        Event::ChannelOpen { ch } => ("open", Some(*ch)),
        Event::Exec { ch, command } => {
            set(&o, "command", bytes(command));
            ("exec", Some(*ch))
        }
        Event::Shell { ch } => ("shell", Some(*ch)),
        Event::Subsystem { ch, name } => {
            set(&o, "name", name.as_str());
            ("subsystem", Some(*ch))
        }
        Event::Data { ch } => ("data", Some(*ch)),
        Event::Eof { ch } => ("eof", Some(*ch)),
        Event::Close { ch } => ("close", Some(*ch)),
    };
    set(&o, "type", typ);
    if let Some(ch) = ch {
        set(&o, "ch", ch);
    }
    o
}

/// The host key's line for `known_hosts` (`ssh-ed25519 AAAA…`), from its
/// OpenSSH PEM. Throws `key`.
#[wasm_bindgen(js_name = sshHostKeyLine)]
pub fn ssh_host_key_line(pem: &str) -> Result<String, JsValue> {
    ssh::HostKey::from_openssh(pem)
        .map(|k| k.public_line())
        .map_err(|e| js_err(&e))
}

/// A public key from a line of `authorized_keys` (without options) or a
/// `.pub` file. Throws `key`.
#[wasm_bindgen(js_name = sshPublicKey, unchecked_return_type = "SshKey")]
pub fn ssh_public_key(line: &str) -> Result<JsValue, JsValue> {
    PublicKey::from_line(line)
        .map(|k| key_value(&k).into())
        .map_err(|e| js_err(&e))
}

/// A host key and the SSH settings, shared by connections.
#[wasm_bindgen]
pub struct SshConfig(ssh::Config);

#[wasm_bindgen]
impl SshConfig {
    /// `hostKeyPem`: an unencrypted OpenSSH Ed25519 key, as `ssh-keygen -t
    /// ed25519 -N ''` writes it. Throws `key` or `config`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        host_key_pem: &str,
        #[wasm_bindgen(unchecked_optional_param_type = "SshOptions")] opts: Option<js_sys::Object>,
    ) -> Result<SshConfig, JsValue> {
        let host = ssh::HostKey::from_openssh(host_key_pem).map_err(|e| js_err(&e))?;
        let o = options(opts.map(JsValue::from).unwrap_or(JsValue::UNDEFINED))?;
        ssh::Config::new(host, o)
            .map(SshConfig)
            .map_err(|e| js_err(&e))
    }

    /// The host key's line for `known_hosts`.
    #[wasm_bindgen(getter, js_name = hostKeyLine)]
    pub fn host_key_line(&self) -> String {
        self.0.host_key().public_line()
    }
}

fn options(v: JsValue) -> Result<ssh::Options, JsValue> {
    let mut o = ssh::Options::default();
    if v.is_undefined() || v.is_null() {
        return Ok(o);
    }
    let get =
        |k: &str| js_sys::Reflect::get(&v, &JsValue::from_str(k)).unwrap_or(JsValue::UNDEFINED);
    let int = |k: &str, max: f64| -> Result<Option<u64>, JsValue> {
        let x = get(k);
        if x.is_undefined() {
            return Ok(None);
        }
        x.as_f64()
            .filter(|n| n.fract() == 0.0 && *n >= 1.0 && *n <= max)
            .map(|n| Some(n as u64))
            .ok_or_else(|| config_err(format!("{k}: a positive integer")))
    };
    let names = |k: &str| -> Result<Option<Vec<String>>, JsValue> {
        let x = get(k);
        if x.is_undefined() {
            return Ok(None);
        }
        let arr = x
            .dyn_into::<js_sys::Array>()
            .map_err(|_| config_err(format!("{k}: not an array")))?;
        arr.iter()
            .map(|n| {
                n.as_string()
                    .ok_or_else(|| config_err(format!("{k}: not a string")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    };
    if let Some(w) = int("window", u32::MAX as f64)? {
        o.window = w as u32;
    }
    if let Some(b) = int("rekeyBytes", 9e15)? {
        o.rekey_bytes = b;
    }
    if let Some(s) = int("rekeySecs", 9e15)? {
        o.rekey_secs = s;
    }
    if let Some(ks) = names("kex")? {
        o.kex = ks
            .iter()
            .map(|n| {
                [Kex::MlKem768X25519, Kex::Curve25519]
                    .into_iter()
                    .find(|k| k.name() == n)
                    .ok_or_else(|| config_err(format!("kex: unknown {n}")))
            })
            .collect::<Result<_, _>>()?;
    }
    if let Some(cs) = names("ciphers")? {
        o.ciphers = cs
            .iter()
            .map(|n| {
                [Cipher::Chacha20Poly1305, Cipher::Aes256Gcm]
                    .into_iter()
                    .find(|c| c.name() == n)
                    .ok_or_else(|| config_err(format!("ciphers: unknown {n}")))
            })
            .collect::<Result<_, _>>()?;
    }
    let sw = get("software");
    if !sw.is_undefined() {
        o.software = sw
            .as_string()
            .ok_or_else(|| config_err("software: not a string"))?;
    }
    Ok(o)
}

/// Unix seconds from a JS number.
fn secs(now: f64) -> u64 {
    if now.is_finite() && now > 0.0 {
        now as u64
    } else {
        0
    }
}

fn write_value(taken: usize, out: &[u8]) -> JsValue {
    let o = js_sys::Object::new();
    set(&o, "taken", taken as u32);
    set(&o, "out", bytes(out));
    o.into()
}

/// One SSH connection, server side.
#[wasm_bindgen]
pub struct SshConn {
    c: ssh::Conn,
    /// The greeting of a new connection, sent with the first `input`.
    greeting: Vec<u8>,
}

#[wasm_bindgen]
impl SshConn {
    /// A new connection at Unix time `now` (seconds). The server speaks
    /// first: its version line and KEXINIT come out of the first `input`.
    pub fn start(config: &SshConfig, now: f64) -> SshConn {
        let (c, greeting) = ssh::Conn::new(&config.0, secs(now));
        SshConn { c, greeting }
    }

    /// A connection from its `state()`. Throws `state`.
    pub fn restore(config: &SshConfig, state: &[u8]) -> Result<SshConn, JsValue> {
        ssh::Conn::restore(&config.0, state)
            .map(|c| SshConn {
                c,
                greeting: Vec::new(),
            })
            .map_err(|e| js_err(&e))
    }

    /// Processes input from the client. `buf` starts at the first byte not
    /// yet consumed; `eof`: the client's input ends after it; `now`: Unix
    /// seconds.
    #[wasm_bindgen(unchecked_return_type = "SshProgress")]
    pub fn input(&mut self, buf: &[u8], eof: bool, now: f64) -> JsValue {
        let p = self.c.input(buf, eof, secs(now));
        let mut out = std::mem::take(&mut self.greeting);
        out.extend_from_slice(&p.out);
        let o = js_sys::Object::new();
        set(&o, "consumed", p.consumed as u32);
        set(&o, "out", bytes(&out));
        let events = js_sys::Array::new();
        for e in &p.events {
            events.push(&event_value(e));
        }
        set(&o, "events", events);
        set(&o, "peerClosed", p.peer_closed);
        set(&o, "held", p.held);
        if let Some(e) = &p.error {
            set(&o, "error", failure(e));
        }
        o.into()
    }

    /// The question waiting for `authorize`, if any (after a restore, the
    /// application asks again).
    #[wasm_bindgen(js_name = pendingAuthorize, unchecked_return_type = "SshAuthorize | undefined")]
    pub fn pending_authorize(&self) -> JsValue {
        match self.c.pending_authorize() {
            Some(a) => {
                let o = js_sys::Object::new();
                authorize_value(&a, &o);
                o.into()
            }
            None => JsValue::UNDEFINED,
        }
    }

    /// Answers the question: the reply to send. Throws `not_asked` or
    /// `closed`.
    pub fn authorize(&mut self, ok: bool) -> Result<Vec<u8>, JsValue> {
        self.c.authorize(ok).map_err(|e| js_err(&e))
    }

    /// The open channels.
    #[wasm_bindgen(unchecked_return_type = "SshChannelInfo[]")]
    pub fn channels(&self) -> JsValue {
        let arr = js_sys::Array::new();
        for c in self.c.channels() {
            let o = js_sys::Object::new();
            set(&o, "ch", c.ch);
            let req = match c.request {
                Some(Request::Exec) => JsValue::from_str("exec"),
                Some(Request::Shell) => JsValue::from_str("shell"),
                Some(Request::Subsystem) => JsValue::from_str("subsystem"),
                None => JsValue::NULL,
            };
            set(&o, "request", req);
            set(&o, "eof", c.eof);
            set(&o, "closed", c.closed);
            set(&o, "unread", c.unread as u32);
            set(&o, "queued", c.queued as u32);
            set(&o, "peerWindow", c.peer_window);
            arr.push(&o);
        }
        arr.into()
    }

    /// What the client sent on `ch` and the application hasn't consumed.
    #[wasm_bindgen(js_name = channelData)]
    pub fn channel_data(&self, ch: u32) -> Vec<u8> {
        self.c.channel_data(ch).to_vec()
    }

    /// Consumes `n` bytes of `channelData(ch)`; the output is a
    /// WINDOW_ADJUST when one is due. Throws `no_channel` or `config`.
    pub fn consume(&mut self, ch: u32, n: u32) -> Result<Vec<u8>, JsValue> {
        self.c.consume(ch, n as usize).map_err(|e| js_err(&e))
    }

    /// Writes standard output: how much it took (what the client's window
    /// takes now plus what fits the queue), and the bytes to send.
    #[wasm_bindgen(unchecked_return_type = "SshWrite")]
    pub fn write(&mut self, ch: u32, data: &[u8]) -> Result<JsValue, JsValue> {
        let (n, out) = self.c.write(ch, data).map_err(|e| js_err(&e))?;
        Ok(write_value(n, &out))
    }

    /// Writes standard error.
    #[wasm_bindgen(js_name = writeStderr, unchecked_return_type = "SshWrite")]
    pub fn write_stderr(&mut self, ch: u32, data: &[u8]) -> Result<JsValue, JsValue> {
        let (n, out) = self.c.write_stderr(ch, data).map_err(|e| js_err(&e))?;
        Ok(write_value(n, &out))
    }

    /// Sends `exit-status` after what is queued.
    #[wasm_bindgen(js_name = exitStatus)]
    pub fn exit_status(&mut self, ch: u32, code: u32) -> Result<Vec<u8>, JsValue> {
        self.c.exit_status(ch, code).map_err(|e| js_err(&e))
    }

    /// Sends EOF after what is queued.
    pub fn eof(&mut self, ch: u32) -> Result<Vec<u8>, JsValue> {
        self.c.eof(ch).map_err(|e| js_err(&e))
    }

    /// Closes the channel after what is queued.
    pub fn close(&mut self, ch: u32) -> Result<Vec<u8>, JsValue> {
        self.c.close(ch).map_err(|e| js_err(&e))
    }

    /// Starts a key exchange now, if none is in progress.
    pub fn rekey(&mut self) -> Vec<u8> {
        self.c.rekey()
    }

    /// Ends the connection with DISCONNECT (`reason`: RFC 4253 §11.1; 11
    /// is "by application").
    pub fn disconnect(&mut self, reason: u32, message: &str) -> Vec<u8> {
        self.c.disconnect(reason, message)
    }

    /// The state to save with the step.
    pub fn state(&self) -> Vec<u8> {
        self.c.state()
    }

    /// The authenticated user.
    #[wasm_bindgen(getter)]
    pub fn user(&self) -> Option<String> {
        self.c.user().map(str::to_owned)
    }

    /// The connection has ended (either side).
    #[wasm_bindgen(getter)]
    pub fn ended(&self) -> bool {
        self.c.ended()
    }

    /// A key exchange is in progress.
    #[wasm_bindgen(getter, js_name = inKex)]
    pub fn in_kex(&self) -> bool {
        self.c.in_kex()
    }

    /// Key exchanges completed.
    #[wasm_bindgen(getter)]
    pub fn kexes(&self) -> f64 {
        self.c.kexes() as f64
    }

    /// The negotiated ciphers, `[client to server, server to client]`, once
    /// the first key exchange is done.
    #[wasm_bindgen(getter)]
    pub fn ciphers(&self) -> Option<Vec<String>> {
        self.c
            .ciphers()
            .map(|(a, b)| vec![a.name().to_owned(), b.name().to_owned()])
    }
}
