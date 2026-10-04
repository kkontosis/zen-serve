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
    /// Server limits.
    pub limits: Limits,
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
    /// Idempotency record lifetime.
    pub idempotency_ttl_secs: u64,
    /// Session lifetime.
    pub session_ttl_secs: u64,
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
    /// The device's fingerprint.
    #[serde(with = "serde_bytes")]
    pub device_fp: Vec<u8>,
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
    /// Server-side CRDT ops (not implemented yet).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crdt_ops: Vec<ciborium::Value>,
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
