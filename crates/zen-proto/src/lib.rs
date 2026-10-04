//! zen-proto: wire types of the zen-serve API (spec/api.md) and the signed
//! ACL document (spec/formats.md §9). Shared by the server and clients;
//! wasm-compatible (no I/O, no runtime).
//!
//! Bodies are CBOR. Byte fields are CBOR byte strings; optional fields are
//! omitted when absent.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub use ciborium::Value as CborValue;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
pub use serde_bytes::ByteBuf;

pub mod acl;

/// API version.
pub const API_VERSION: u32 = 1;
/// CBOR media type.
pub const CBOR: &str = "application/cbor";
/// Length of a commit id.
pub const COMMIT_ID_LEN: usize = 16;
/// Length of a value version (versionstamp).
pub const VERSION_LEN: usize = 10;
/// Length of an event offset.
pub const OFFSET_LEN: usize = 12;
/// Length of an event key token.
pub const KEY_TOKEN_LEN: usize = 16;
/// The offset before the first event.
pub const ZERO_OFFSET: [u8; OFFSET_LEN] = [0; OFFSET_LEN];

/// Encode a value as CBOR.
pub fn to_cbor<T: Serialize>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(v, &mut out).expect("CBOR encoding to a Vec cannot fail");
    out
}

/// Decode CBOR.
pub fn from_cbor<T: DeserializeOwned>(b: &[u8]) -> Result<T, String> {
    ciborium::from_reader(b).map_err(|e| e.to_string())
}

/// An error body: `{code, message}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Machine-readable code (spec/api.md §1).
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

/// An empty request or response body: `{}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Empty {}

// ---------------------------------------------------------------- info

/// `GET /v1/info`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Info {
    /// Server name and version.
    pub server: String,
    /// API version.
    pub api: u32,
    /// Supported suite ids.
    pub suites: Vec<u8>,
    /// Supported format versions.
    pub formats: Vec<u8>,
    /// Feature names.
    pub features: Vec<String>,
    /// Whether COOP/COEP/CORP are sent.
    pub cross_origin_isolation: bool,
    /// Whether an ACL exists.
    pub claimed: bool,
    /// Server clock, unix milliseconds (HLC observation, spec/fs.md §2).
    #[serde(default)]
    pub time_ms: u64,
    /// Server limits.
    pub limits: Limits,
    /// Sign-in methods and origin policy (spec/auth.md). Absent from
    /// servers older than the multi-method sign-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthInfo>,
}

/// `/v1/info` `auth`: what a client needs to sign in (spec/auth.md §2).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AuthInfo {
    /// The sign-in methods this server offers: enabled and implemented
    /// ([`AuthMethod::name`]).
    pub methods: Vec<String>,
    /// The method a client offers first, if any is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// A sign-in method (spec/auth.md §1). The discriminant is its id, stored
/// in session records and credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AuthMethod {
    /// 1: a per-device hybrid key certified in the signed ACL.
    DeviceKey = 1,
    /// 2: WebAuthn passkeys (reserved).
    Passkey = 2,
    /// 3: OPAQUE password authentication (reserved).
    Opaque = 3,
    /// 4: admin-issued bearer tokens for services and bots.
    ApiToken = 4,
    /// 5: TLS client certificates (reserved).
    Mtls = 5,
    /// 6: a hybrid key derived from a password on the client.
    PasswordKey = 6,
}

impl AuthMethod {
    /// Every method, by id.
    pub const ALL: [AuthMethod; 6] = [
        AuthMethod::DeviceKey,
        AuthMethod::Passkey,
        AuthMethod::Opaque,
        AuthMethod::ApiToken,
        AuthMethod::Mtls,
        AuthMethod::PasswordKey,
    ];

    /// The method's id.
    pub fn id(self) -> u8 {
        self as u8
    }

    /// The method with this id.
    pub fn from_id(id: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.id() == id)
    }

    /// The wire name, as in `/v1/info`.
    pub fn name(self) -> &'static str {
        match self {
            AuthMethod::DeviceKey => "device_key",
            AuthMethod::Passkey => "passkey",
            AuthMethod::Opaque => "opaque",
            AuthMethod::ApiToken => "api_token",
            AuthMethod::Mtls => "mtls",
            AuthMethod::PasswordKey => "password_key",
        }
    }

    /// The method with this wire name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.name() == name)
    }
}

/// Server limits (spec/api.md §2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    /// Max stored-key length.
    pub max_key_bytes: u32,
    /// Max sealed value length.
    pub max_value_bytes: u32,
    /// Max event envelope length.
    pub max_envelope_bytes: u32,
    /// Max total payload of one commit.
    pub max_commit_bytes: u32,
    /// Max number of operations in one commit.
    pub max_commit_ops: u32,
    /// Max items per range read (and per `expect_ranges` range).
    pub max_range_items: u32,
    /// Max bytes per range read.
    #[serde(default)]
    pub max_range_bytes: u64,
    /// Idempotency record lifetime.
    pub idempotency_ttl_secs: u64,
    /// Session lifetime.
    pub session_ttl_secs: u64,
    /// How far an `hlc` may be ahead of the server clock.
    #[serde(default)]
    pub crdt_max_skew_ms: u64,
    /// How far back a late filesystem operation may reach.
    #[serde(default)]
    pub crdt_horizon_secs: u64,
    /// Max logged moves one late move may undo and redo.
    #[serde(default)]
    pub crdt_max_redo: u32,
    /// Max depth of a move's new parent.
    #[serde(default)]
    pub crdt_max_depth: u32,
    /// How long an unreferenced chunk is kept.
    #[serde(default)]
    pub chunk_grace_secs: u64,
    /// `per_key` claim lifetime.
    #[serde(default)]
    pub claim_ttl_ms: u32,
    /// How long ephemeral messages stay in the cross-node ring.
    #[serde(default)]
    pub ephemeral_ttl_secs: u64,
    /// Max consumer groups per topic.
    #[serde(default)]
    pub max_groups_per_topic: u32,
    /// Ephemeral publishes per device and node: sustained bytes per second
    /// (a message costs its data plus 256 bytes); 0 = no limit.
    #[serde(default)]
    pub ephemeral_bytes_per_sec: u64,
    /// Ephemeral publishes per device and node: burst, in bytes.
    #[serde(default)]
    pub ephemeral_burst_bytes: u64,
}

// ---------------------------------------------------------------- auth

/// `POST /v1/auth/challenge` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Challenge {
    /// 32 random bytes.
    #[serde(with = "serde_bytes")]
    pub challenge: Vec<u8>,
}

/// `POST /v1/auth/session` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionRequest {
    /// The challenge.
    #[serde(with = "serde_bytes")]
    pub challenge: Vec<u8>,
    /// The server origin as the client sees it.
    pub origin: String,
    /// Encoded user public identity.
    #[serde(with = "serde_bytes")]
    pub user: Vec<u8>,
    /// Device certificate.
    #[serde(with = "serde_bytes")]
    pub cert: Vec<u8>,
    /// Device signature (purpose `zen/v1/sig/session`).
    #[serde(with = "serde_bytes")]
    pub sig: Vec<u8>,
}

/// `POST /v1/auth/session` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Bearer token (send base64url without padding).
    #[serde(with = "serde_bytes")]
    pub token: Vec<u8>,
    /// Expiry, unix seconds.
    pub expires_unix: u64,
    /// The user's fingerprint.
    #[serde(with = "serde_bytes")]
    pub user_fp: Vec<u8>,
    /// The device's fingerprint; for methods other than `device_key`, the
    /// credential id (spec/auth.md §3).
    #[serde(with = "serde_bytes")]
    pub device_fp: Vec<u8>,
    /// The sign-in method ([`AuthMethod::name`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

/// Max length of an origin.
pub const MAX_ORIGIN_LEN: usize = 255;

/// Whether `o` is a serialized origin as browsers send it (spec/auth.md
/// §5): `http://` or `https://`, a lowercase ASCII host (a name, an IPv4
/// address or a bracketed IPv6 address), an optional port 1–65535, and
/// nothing else, not even a trailing slash.
pub fn valid_origin(o: &str) -> bool {
    if o.len() > MAX_ORIGIN_LEN {
        return false;
    }
    let Some(rest) = o
        .strip_prefix("https://")
        .or_else(|| o.strip_prefix("http://"))
    else {
        return false;
    };
    let (host, port) = if let Some(v6) = rest.strip_prefix('[') {
        let Some((h, after)) = v6.split_once(']') else {
            return false;
        };
        if h.is_empty()
            || !h
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
        {
            return false;
        }
        match after {
            "" => (h, None),
            p => match p.strip_prefix(':') {
                Some(p) => (h, Some(p)),
                None => return false,
            },
        }
    } else {
        let (h, p) = match rest.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (rest, None),
        };
        let ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.';
        if h.is_empty() || !h.bytes().all(ok) {
            return false;
        }
        (h, p)
    };
    if host.bytes().any(|b| b.is_ascii_uppercase()) {
        return false;
    }
    match port {
        None => true,
        Some(p) => {
            !p.is_empty()
                && p.len() <= 5
                && p.bytes().all(|b| b.is_ascii_digit())
                && !p.starts_with('0')
                && p.parse::<u32>().is_ok_and(|n| (1..=65535).contains(&n))
        }
    }
}

/// The session-signature message: `lp(challenge) ‖ lp(origin)`
/// (spec/formats.md §10).
pub fn session_message(challenge: &[u8], origin: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(8 + challenge.len() + origin.len());
    m.extend_from_slice(&(challenge.len() as u32).to_be_bytes());
    m.extend_from_slice(challenge);
    m.extend_from_slice(&(origin.len() as u32).to_be_bytes());
    m.extend_from_slice(origin.as_bytes());
    m
}

// ---------------------------------------------------------------- ACL, fs

/// `POST /v1/acl/put` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AclPut {
    /// CBOR-encoded [`acl::SignedAcl`].
    #[serde(with = "serde_bytes")]
    pub acl: Vec<u8>,
    /// Claim token, for version 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<String>,
}

/// `POST /v1/acl/put` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AclVersion {
    /// The new head version.
    pub version: u64,
}

/// `POST /v1/acl/get` request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AclGet {
    /// First version to return (default: head).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<u64>,
}

/// `POST /v1/acl/get` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AclEntries {
    /// Head version (0 if unclaimed).
    pub head: u64,
    /// Signed ACLs, oldest first.
    pub entries: Vec<ByteBuf>,
}

/// One fs in `POST /v1/fs/list`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FsEntry {
    /// fs_id.
    pub id: u32,
    /// The caller's rights: `read`, `write`, `topics`, `admin`.
    pub rights: Vec<String>,
}

/// `POST /v1/fs/list` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FsList {
    /// Accessible filesystems.
    pub fs: Vec<FsEntry>,
}

/// `POST /v1/fs/header/get` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HeaderGet {
    /// fs_id.
    pub fs: u32,
}

/// `POST /v1/fs/header/get` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Header {
    /// The opaque header, if set.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub header: Option<Vec<u8>>,
    /// Its version.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub version: Option<Vec<u8>>,
}

/// `POST /v1/fs/header/put` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HeaderPut {
    /// fs_id.
    pub fs: u32,
    /// The opaque header.
    #[serde(with = "serde_bytes")]
    pub header: Vec<u8>,
    /// Expected current version; absent = must not exist.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub expect: Option<Vec<u8>>,
}

/// `POST /v1/fs/header/put` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HeaderVersion {
    /// The new version.
    #[serde(with = "serde_bytes")]
    pub version: Vec<u8>,
}

// ---------------------------------------------------------------- KV

/// `POST /v1/grv` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadVersion {
    /// The read version.
    pub read_version: u64,
}

/// `POST /v1/kv/get` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KvGet {
    /// fs_id.
    pub fs: u32,
    /// Stored keys.
    pub keys: Vec<ByteBuf>,
    /// Snapshot to read at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_version: Option<u64>,
}

/// `POST /v1/kv/range` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KvRange {
    /// fs_id.
    pub fs: u32,
    /// Inclusive start.
    #[serde(with = "serde_bytes")]
    pub begin: Vec<u8>,
    /// Exclusive end; absent = end of the fs.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub end: Option<Vec<u8>>,
    /// Max items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Read backwards from `end`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse: Option<bool>,
    /// Snapshot to read at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_version: Option<u64>,
}

/// One KV item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KvItem {
    /// Stored key.
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    /// Sealed value, or null if absent.
    #[serde(default, with = "serde_bytes")]
    pub value: Option<Vec<u8>>,
    /// Value version, or null if absent.
    #[serde(default, with = "serde_bytes")]
    pub version: Option<Vec<u8>>,
}

/// `POST /v1/kv/get` and `/v1/kv/range` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KvItems {
    /// The snapshot read.
    pub read_version: u64,
    /// Items.
    pub items: Vec<KvItem>,
    /// Range only: the limit cut the range short.
    #[serde(default)]
    pub more: bool,
}

// ---------------------------------------------------------------- commit

/// A key range in one fs; `end` absent = end of the fs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FsRange {
    /// fs_id.
    pub fs: u32,
    /// Inclusive start.
    #[serde(with = "serde_bytes")]
    pub begin: Vec<u8>,
    /// Exclusive end.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub end: Option<Vec<u8>>,
}

/// Long-mode per-key expectation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Expect {
    /// fs_id.
    pub fs: u32,
    /// Stored key.
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    /// Expected version; null = must be absent.
    #[serde(default, with = "serde_bytes")]
    pub version: Option<Vec<u8>>,
}

/// Long-mode range expectation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExpectRange {
    /// fs_id.
    pub fs: u32,
    /// Inclusive start.
    #[serde(with = "serde_bytes")]
    pub begin: Vec<u8>,
    /// Exclusive end.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub end: Option<Vec<u8>>,
    /// Range hash (zen_core::kdf::RangeHasher).
    #[serde(with = "serde_bytes")]
    pub hash: Vec<u8>,
}

/// A KV write; `value` null = delete.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Write {
    /// fs_id.
    pub fs: u32,
    /// Stored key.
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    /// Sealed value.
    #[serde(default, with = "serde_bytes")]
    pub value: Option<Vec<u8>>,
}

/// An event append.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Append {
    /// fs_id.
    pub fs: u32,
    /// Topic id.
    #[serde(with = "serde_bytes")]
    pub topic: Vec<u8>,
    /// Event key token.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub key_token: Option<Vec<u8>>,
    /// Sealed event.
    #[serde(with = "serde_bytes")]
    pub envelope: Vec<u8>,
}

/// A consume step (spec/api.md §8.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Consume {
    /// fs_id.
    pub fs: u32,
    /// Group name.
    #[serde(with = "serde_bytes")]
    pub group: Vec<u8>,
    /// Partition (lease modes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<u32>,
    /// Key token (`per_key`).
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub key_token: Option<Vec<u8>>,
    /// Expected current cursor.
    #[serde(with = "serde_bytes")]
    pub from: Vec<u8>,
    /// Offset of the event being consumed.
    #[serde(with = "serde_bytes")]
    pub to: Vec<u8>,
    /// Lease or claim token.
    pub token: u64,
}

/// `POST /v1/commit` request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Commit {
    /// 16 random bytes.
    #[serde(with = "serde_bytes")]
    pub commit_id: Vec<u8>,
    /// Short mode read version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_version: Option<u64>,
    /// Short mode read conflict ranges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_conflicts: Vec<FsRange>,
    /// Long mode key expectations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expect: Vec<Expect>,
    /// Long mode range expectations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expect_ranges: Vec<ExpectRange>,
    /// KV writes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writes: Vec<Write>,
    /// KV range clears.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear_ranges: Vec<FsRange>,
    /// Event appends.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub append: Vec<Append>,
    /// Consume steps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consume: Vec<Consume>,
    /// Filesystem chunks (spec/fs.md §4.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<ChunkPut>,
    /// Filesystem operations (spec/fs.md §3, §4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crdt_ops: Vec<CrdtOp>,
}

/// `POST /v1/commit` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommitResult {
    /// Commit version.
    pub commit_version: u64,
    /// Commit versionstamp (10 bytes).
    #[serde(with = "serde_bytes")]
    pub versionstamp: Vec<u8>,
    /// Offsets of the appended events, in request order.
    pub appended: Vec<ByteBuf>,
    /// Dots of the versions created by `write` operations, in request order.
    #[serde(default)]
    pub dots: Vec<ByteBuf>,
}

/// `POST /v1/log/append` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogAppend {
    /// 16 random bytes.
    #[serde(with = "serde_bytes")]
    pub commit_id: Vec<u8>,
    /// Appends.
    pub append: Vec<Append>,
}

// ---------------------------------------------------------------- log

/// `POST /v1/log/read` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogRead {
    /// fs_id.
    pub fs: u32,
    /// Topic id.
    #[serde(with = "serde_bytes")]
    pub topic: Vec<u8>,
    /// Exclusive start offset.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub after: Option<Vec<u8>>,
    /// Only this key's events.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub key_token: Option<Vec<u8>>,
    /// Max events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One stored event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Offset (12 bytes).
    #[serde(with = "serde_bytes")]
    pub offset: Vec<u8>,
    /// Event key token.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub key_token: Option<Vec<u8>>,
    /// Sealed event.
    #[serde(with = "serde_bytes")]
    pub envelope: Vec<u8>,
}

/// `POST /v1/log/read` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogEvents {
    /// Events, in offset order.
    pub events: Vec<Event>,
    /// More events follow.
    pub more: bool,
}

// ---------------------------------------------------------------- consume

/// Consumer group mode (DESIGN-4 §1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// No server state; every subscriber reads everything.
    Broadcast,
    /// One cursor, delivery gate.
    Sequential,
    /// N cursors, sequential per partition.
    Partitioned,
    /// Sequential per key, parallel across keys.
    PerKey,
    /// Sequential over one key.
    SingleKey,
}

impl Mode {
    /// Mode byte in the keyspace (spec/keyspace.md §3.3).
    pub fn byte(self) -> u8 {
        match self {
            Mode::Broadcast => 1,
            Mode::Sequential => 2,
            Mode::Partitioned => 3,
            Mode::PerKey => 4,
            Mode::SingleKey => 5,
        }
    }
}

/// What to do with a poison event (G7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnPoison {
    /// Dead-letter it and move on.
    Dlq,
    /// Redeliver forever.
    Block,
}

/// Where a new group starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Start {
    /// From the first event.
    Earliest,
    /// After the current last event.
    Latest,
}

/// `POST /v1/consume/groups` request; also the stored group definition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupDef {
    /// fs_id.
    pub fs: u32,
    /// Group name (1..=64 bytes).
    #[serde(with = "serde_bytes")]
    pub group: Vec<u8>,
    /// Topic id.
    #[serde(with = "serde_bytes")]
    pub topic: Vec<u8>,
    /// Mode.
    pub mode: Mode,
    /// Partition count (`partitioned`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partitions: Option<u32>,
    /// Key token (`single_key`).
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub key_token: Option<Vec<u8>>,
    /// Events per `next` (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_inflight: Option<u32>,
    /// Attempts before poison handling (default 5; 0 = unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    /// Poison handling (default `dlq`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_poison: Option<OnPoison>,
    /// Starting point (default `earliest`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<Start>,
}

/// `POST /v1/consume/groups` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupCreated {
    /// False if an identical group already existed.
    pub created: bool,
}

/// Addresses one cursor of a group: a partition, or a key (`per_key`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupRef {
    /// fs_id.
    pub fs: u32,
    /// Group name.
    #[serde(with = "serde_bytes")]
    pub group: Vec<u8>,
    /// Partition (lease modes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<u32>,
    /// Key token (`per_key`).
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub key_token: Option<Vec<u8>>,
}

/// `POST /v1/consume/lease` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeaseRequest {
    /// fs_id.
    pub fs: u32,
    /// Group name.
    #[serde(with = "serde_bytes")]
    pub group: Vec<u8>,
    /// Partition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<u32>,
    /// Current token, to renew.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<u64>,
    /// Lease lifetime (default 10 000 ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u32>,
}

/// `POST /v1/consume/lease` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    /// Fencing token.
    pub token: u64,
    /// Expiry version.
    pub expires_version: u64,
    /// Committed cursor.
    #[serde(with = "serde_bytes")]
    pub cursor: Vec<u8>,
}

/// `POST /v1/consume/release` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeaseRelease {
    /// fs_id.
    pub fs: u32,
    /// Group name.
    #[serde(with = "serde_bytes")]
    pub group: Vec<u8>,
    /// Partition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<u32>,
    /// Lease token.
    pub token: u64,
}

/// `POST /v1/consume/next` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NextRequest {
    /// fs_id.
    pub fs: u32,
    /// Group name.
    #[serde(with = "serde_bytes")]
    pub group: Vec<u8>,
    /// Partition (lease modes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<u32>,
    /// Lease token (lease modes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<u64>,
    /// Max events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Long-poll timeout (≤ 30 000 ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u32>,
}

/// One delivered event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    /// Offset.
    #[serde(with = "serde_bytes")]
    pub offset: Vec<u8>,
    /// Event key token.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub key_token: Option<Vec<u8>>,
    /// Sealed event.
    #[serde(with = "serde_bytes")]
    pub envelope: Vec<u8>,
    /// The `from` to present in the consume step.
    #[serde(with = "serde_bytes")]
    pub from: Vec<u8>,
    /// Lease or claim token to present.
    pub token: u64,
    /// Failed attempts so far.
    pub attempts: u32,
}

/// `POST /v1/consume/next` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Deliveries {
    /// Delivered events.
    pub events: Vec<Delivery>,
}

/// `POST /v1/consume/nack` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Nack {
    /// fs_id.
    pub fs: u32,
    /// Group name.
    #[serde(with = "serde_bytes")]
    pub group: Vec<u8>,
    /// Partition (lease modes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<u32>,
    /// Key token (`per_key`).
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub key_token: Option<Vec<u8>>,
    /// The failed event.
    #[serde(with = "serde_bytes")]
    pub offset: Vec<u8>,
    /// Lease or claim token.
    pub token: u64,
}

/// `POST /v1/consume/nack` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NackResult {
    /// Attempts so far.
    pub attempts: u32,
    /// The event went to the DLQ.
    pub dead_lettered: bool,
}

/// `POST /v1/consume/cursor` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    /// Committed cursor.
    #[serde(with = "serde_bytes")]
    pub cursor: Vec<u8>,
    /// Oldest pending offset (`per_key`).
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub low_watermark: Option<Vec<u8>>,
}

/// `POST /v1/consume/dlq/list` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DlqList {
    /// fs_id.
    pub fs: u32,
    /// Group name.
    #[serde(with = "serde_bytes")]
    pub group: Vec<u8>,
    /// Exclusive start id.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub after: Option<Vec<u8>>,
    /// Max items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One dead-lettered event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DlqItem {
    /// DLQ entry id.
    #[serde(with = "serde_bytes")]
    pub id: Vec<u8>,
    /// Original offset.
    #[serde(with = "serde_bytes")]
    pub offset: Vec<u8>,
    /// Topic id.
    #[serde(with = "serde_bytes")]
    pub topic: Vec<u8>,
    /// Event key token.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub key_token: Option<Vec<u8>>,
    /// Sealed event.
    #[serde(with = "serde_bytes")]
    pub envelope: Vec<u8>,
}

/// `POST /v1/consume/dlq/list` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DlqItems {
    /// Items, oldest first.
    pub items: Vec<DlqItem>,
}

/// `POST /v1/consume/dlq/retry` and `/drop` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DlqOp {
    /// fs_id.
    pub fs: u32,
    /// Group name.
    #[serde(with = "serde_bytes")]
    pub group: Vec<u8>,
    /// DLQ entry id.
    #[serde(with = "serde_bytes")]
    pub id: Vec<u8>,
    /// Commit id (retry only).
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub commit_id: Option<Vec<u8>>,
}

// ---------------------------------------------------------------- filesystem

/// A chunk upload in a commit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkPut {
    /// fs_id.
    pub fs: u32,
    /// Chunk id (16 bytes).
    #[serde(with = "serde_bytes")]
    pub id: Vec<u8>,
    /// Sealed chunk.
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// A filesystem operation in a commit, tagged by `op` (spec/api.md §6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CrdtOp {
    /// Create, move, rename+move, delete (to trash), restore.
    Move {
        /// fs_id.
        fs: u32,
        /// Tree id.
        #[serde(with = "serde_bytes")]
        tree: Vec<u8>,
        /// Node id.
        #[serde(with = "serde_bytes")]
        node: Vec<u8>,
        /// New parent.
        #[serde(with = "serde_bytes")]
        parent: Vec<u8>,
        /// Hybrid logical clock.
        hlc: u64,
        /// Sealed meta, applied with the same timestamp.
        #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
        meta: Option<Vec<u8>>,
    },
    /// Set a node's sealed meta (LWW).
    Meta {
        /// fs_id.
        fs: u32,
        /// Tree id.
        #[serde(with = "serde_bytes")]
        tree: Vec<u8>,
        /// Node id.
        #[serde(with = "serde_bytes")]
        node: Vec<u8>,
        /// Hybrid logical clock.
        hlc: u64,
        /// Sealed meta.
        #[serde(with = "serde_bytes")]
        meta: Vec<u8>,
    },
    /// Write a content version (multi-value register).
    Write {
        /// fs_id.
        fs: u32,
        /// Tree id.
        #[serde(with = "serde_bytes")]
        tree: Vec<u8>,
        /// Node id.
        #[serde(with = "serde_bytes")]
        node: Vec<u8>,
        /// Dots of the versions this one replaces.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        replaces: Vec<ByteBuf>,
        /// Chunk ids in file order.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        chunks: Vec<ByteBuf>,
        /// Sealed manifest.
        #[serde(with = "serde_bytes")]
        manifest: Vec<u8>,
    },
}

impl CrdtOp {
    /// The op's fs and tree.
    pub fn target(&self) -> (u32, &[u8]) {
        match self {
            CrdtOp::Move { fs, tree, .. }
            | CrdtOp::Meta { fs, tree, .. }
            | CrdtOp::Write { fs, tree, .. } => (*fs, tree),
        }
    }
}

/// A node's state (spec/api.md §12).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeState {
    /// Node id.
    #[serde(with = "serde_bytes")]
    pub node: Vec<u8>,
    /// Parent; absent = invisible.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub parent: Option<Vec<u8>>,
    /// HLC of the move that set the parent.
    pub move_hlc: u64,
    /// Device of that move.
    #[serde(with = "serde_bytes")]
    pub move_device: Vec<u8>,
    /// Sealed meta.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Vec<u8>>,
    /// HLC of the meta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta_hlc: Option<u64>,
    /// Device of the meta.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub meta_device: Option<Vec<u8>>,
    /// Number of content versions.
    pub versions: u32,
    /// Change offset.
    #[serde(with = "serde_bytes")]
    pub changed: Vec<u8>,
}

/// `POST /v1/fs/tree/list` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeList {
    /// fs_id.
    pub fs: u32,
}

/// One tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeEntry {
    /// Tree id.
    #[serde(with = "serde_bytes")]
    pub tree: Vec<u8>,
    /// Operations applied.
    pub ops: u64,
}

/// `POST /v1/fs/tree/list` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Trees {
    /// Trees of the fs.
    pub trees: Vec<TreeEntry>,
}

/// `POST /v1/fs/tree/get` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeGet {
    /// fs_id.
    pub fs: u32,
    /// Tree id.
    #[serde(with = "serde_bytes")]
    pub tree: Vec<u8>,
    /// Node ids.
    pub nodes: Vec<ByteBuf>,
    /// Snapshot to read at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_version: Option<u64>,
}

/// `POST /v1/fs/tree/children` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeChildren {
    /// fs_id.
    pub fs: u32,
    /// Tree id.
    #[serde(with = "serde_bytes")]
    pub tree: Vec<u8>,
    /// Parent node id.
    #[serde(with = "serde_bytes")]
    pub parent: Vec<u8>,
    /// Exclusive start node id.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub after: Option<Vec<u8>>,
    /// Max nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Snapshot to read at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_version: Option<u64>,
}

/// `tree/get` and `tree/children` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Nodes {
    /// The snapshot read.
    pub read_version: u64,
    /// Node states.
    pub nodes: Vec<NodeState>,
    /// `children` only: the limit cut the list short.
    #[serde(default)]
    pub more: bool,
}

/// `POST /v1/fs/tree/changes` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeChanges {
    /// fs_id.
    pub fs: u32,
    /// Tree id.
    #[serde(with = "serde_bytes")]
    pub tree: Vec<u8>,
    /// Exclusive start change offset; absent = full sync.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub after: Option<Vec<u8>>,
    /// Max changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Long-poll up to this long when nothing changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u32>,
}

/// One change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Change {
    /// Change offset.
    #[serde(with = "serde_bytes")]
    pub offset: Vec<u8>,
    /// Node id.
    #[serde(with = "serde_bytes")]
    pub node: Vec<u8>,
    /// Current state; absent = purged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<NodeState>,
}

/// `POST /v1/fs/tree/changes` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Changes {
    /// Changes in offset order.
    pub changes: Vec<Change>,
    /// Offset of the last change returned (or the request's `after`).
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Vec<u8>>,
    /// The limit cut the list short.
    #[serde(default)]
    pub more: bool,
}

/// `POST /v1/fs/tree/chain` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeRef {
    /// fs_id.
    pub fs: u32,
    /// Tree id.
    #[serde(with = "serde_bytes")]
    pub tree: Vec<u8>,
}

/// `POST /v1/fs/tree/chain` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeChain {
    /// Operations applied.
    pub ops: u64,
    /// Op chain head (spec/formats.md §11.5).
    #[serde(with = "serde_bytes")]
    pub chain: Vec<u8>,
}

/// `POST /v1/fs/file/get` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileGet {
    /// fs_id.
    pub fs: u32,
    /// Tree id.
    #[serde(with = "serde_bytes")]
    pub tree: Vec<u8>,
    /// Node id.
    #[serde(with = "serde_bytes")]
    pub node: Vec<u8>,
    /// Snapshot to read at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_version: Option<u64>,
}

/// One content version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Version {
    /// Dot (12 bytes).
    #[serde(with = "serde_bytes")]
    pub dot: Vec<u8>,
    /// Writing device.
    #[serde(with = "serde_bytes")]
    pub device: Vec<u8>,
    /// Chunk ids in file order.
    pub chunks: Vec<ByteBuf>,
    /// Sealed manifest.
    #[serde(with = "serde_bytes")]
    pub manifest: Vec<u8>,
}

/// `POST /v1/fs/file/get` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Versions {
    /// The snapshot read.
    pub read_version: u64,
    /// Versions in dot order (siblings when more than one).
    pub versions: Vec<Version>,
}

/// `POST /v1/fs/chunks/get` request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunksGet {
    /// fs_id.
    pub fs: u32,
    /// Chunk ids.
    pub ids: Vec<ByteBuf>,
}

/// One chunk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkData {
    /// Chunk id.
    #[serde(with = "serde_bytes")]
    pub id: Vec<u8>,
    /// Sealed chunk; absent = unknown.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub data: Option<Vec<u8>>,
}

/// `POST /v1/fs/chunks/get` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chunks {
    /// Chunks in request order.
    pub chunks: Vec<ChunkData>,
}

// ---------------------------------------------------------------- admin

/// `POST /v1/admin/status` (admins only).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ClusterStatus {
    /// `"embedded"` or `"fdb"`.
    pub backend: String,
    /// The database answers reads and writes.
    pub available: bool,
    /// Fully replicated, no degraded processes.
    pub healthy: bool,
    /// FoundationDB redundancy mode (`single`, `double`, `triple`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<String>,
    /// Machines (zen-serve nodes) in the cluster.
    pub machines: u32,
    /// `fdbserver` processes.
    pub processes: u32,
    /// Coordinators.
    pub coordinators: u32,
    /// Cluster messages (warnings).
    pub messages: Vec<String>,
}

// ---------------------------------------------------------------- stream

/// A WebSocket frame (spec/api.md §9), tagged by `op`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Frame {
    /// Client: authenticate.
    Auth {
        /// Session token.
        #[serde(with = "serde_bytes")]
        token: Vec<u8>,
    },
    /// Client: subscribe to a topic or prefix.
    Sub {
        /// Subscription id.
        id: u32,
        /// fs_id.
        fs: u32,
        /// Topic id.
        #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
        topic: Option<Vec<u8>>,
        /// Topic-id prefix.
        #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
        prefix: Option<Vec<u8>>,
        /// Exclusive start offset; absent = live only.
        #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
        after: Option<Vec<u8>>,
    },
    /// Client: stop a subscription.
    Unsub {
        /// Subscription id.
        id: u32,
    },
    /// Client: ephemeral publish.
    Epub {
        /// fs_id.
        fs: u32,
        /// Topic id.
        #[serde(with = "serde_bytes")]
        topic: Vec<u8>,
        /// Opaque data.
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// Client: ephemeral subscribe.
    Esub {
        /// Subscription id.
        id: u32,
        /// fs_id.
        fs: u32,
        /// Topic id.
        #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
        topic: Option<Vec<u8>>,
        /// Topic-id prefix.
        #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
        prefix: Option<Vec<u8>>,
    },
    /// Server: acknowledgement.
    Ok {
        /// Subscription id, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u32>,
    },
    /// Server: a stored event.
    Ev {
        /// Subscription id.
        id: u32,
        /// Topic id.
        #[serde(with = "serde_bytes")]
        topic: Vec<u8>,
        /// Offset.
        #[serde(with = "serde_bytes")]
        offset: Vec<u8>,
        /// Event key token.
        #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
        key_token: Option<Vec<u8>>,
        /// Sealed event.
        #[serde(with = "serde_bytes")]
        envelope: Vec<u8>,
    },
    /// Server: an ephemeral message.
    Eph {
        /// Subscription id.
        id: u32,
        /// Topic id.
        #[serde(with = "serde_bytes")]
        topic: Vec<u8>,
        /// Opaque data.
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        /// Sending device fingerprint.
        #[serde(with = "serde_bytes")]
        sender: Vec<u8>,
    },
    /// Server: an error.
    Err {
        /// Subscription id, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u32>,
        /// Error code.
        code: String,
        /// Message.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_methods_have_stable_ids_and_names() {
        for (i, m) in AuthMethod::ALL.into_iter().enumerate() {
            assert_eq!(m.id() as usize, i + 1);
            assert_eq!(AuthMethod::from_id(m.id()), Some(m));
            assert_eq!(AuthMethod::from_name(m.name()), Some(m));
        }
        assert_eq!(AuthMethod::from_id(0), None);
        assert_eq!(AuthMethod::from_id(7), None);
    }

    #[test]
    fn origins_are_validated() {
        for ok in [
            "https://zen.example.org",
            "http://127.0.0.1:8080",
            "https://[::1]:443",
            "http://localhost",
            "https://a-b.example:65535",
        ] {
            assert!(valid_origin(ok), "{ok}");
        }
        for bad in [
            "",
            "zen.example.org",
            "ftp://zen.example.org",
            "https://",
            "https://zen.example.org/",
            "https://zen.example.org/app",
            "https://Zen.example.org",
            "HTTPS://zen.example.org",
            "https://user@zen.example.org",
            "https://zen.example.org:0",
            "https://zen.example.org:65536",
            "https://zen.example.org:080",
            "https://zen.example.org:",
            "https://zen.example.org?x",
            "https://[::1",
            "https://[::1]x",
            "https://zen example.org",
        ] {
            assert!(!valid_origin(bad), "{bad}");
        }
    }

    #[test]
    fn frames_roundtrip() {
        let frames = [
            Frame::Auth {
                token: vec![1, 2, 3],
            },
            Frame::Sub {
                id: 7,
                fs: 1,
                topic: Some(vec![0; 16]),
                prefix: None,
                after: Some(ZERO_OFFSET.to_vec()),
            },
            Frame::Ev {
                id: 7,
                topic: vec![1; 16],
                offset: vec![2; 12],
                key_token: None,
                envelope: vec![0, 0, 9],
            },
            Frame::Ok { id: None },
        ];
        for f in frames {
            let back: Frame = from_cbor(&to_cbor(&f)).unwrap();
            assert_eq!(back, f);
        }
    }

    #[test]
    fn crdt_ops_roundtrip() {
        let c = Commit {
            commit_id: vec![9; 16],
            chunks: vec![ChunkPut {
                fs: 1,
                id: vec![3; 16],
                data: vec![4; 60],
            }],
            crdt_ops: vec![
                CrdtOp::Move {
                    fs: 1,
                    tree: vec![1; 16],
                    node: vec![2; 16],
                    parent: vec![0; 16],
                    hlc: 1 << 40,
                    meta: Some(vec![5; 50]),
                },
                CrdtOp::Write {
                    fs: 1,
                    tree: vec![1; 16],
                    node: vec![2; 16],
                    replaces: vec![],
                    chunks: vec![ByteBuf::from(vec![3; 16])],
                    manifest: vec![6; 70],
                },
            ],
            ..Default::default()
        };
        let back: Commit = from_cbor(&to_cbor(&c)).unwrap();
        assert_eq!(back, c);
        // Tagged by "op", like stream frames.
        let v: CborValue = from_cbor(&to_cbor(&c.crdt_ops[0])).unwrap();
        let op = v
            .as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_text() == Some("op"))
            .map(|(_, v)| v.as_text().unwrap().to_owned());
        assert_eq!(op.as_deref(), Some("move"));
    }

    #[test]
    fn commit_roundtrip_and_bytes_are_cbor_bytes() {
        let c = Commit {
            commit_id: vec![9; 16],
            writes: vec![Write {
                fs: 1,
                key: vec![1; 16],
                value: None,
            }],
            ..Default::default()
        };
        let enc = to_cbor(&c);
        // commit_id is a CBOR byte string (major type 2, length 16 = 0x50).
        assert!(enc.windows(17).any(|w| w[0] == 0x50 && w[1..] == [9; 16]));
        assert_eq!(from_cbor::<Commit>(&enc).unwrap(), c);
    }
}
