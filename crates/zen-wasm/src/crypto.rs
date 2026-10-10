//! zen-core for JS: handles that own secrets, and free functions over bytes.

use crate::{core_err, js_err, serializer};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use zen_core::fs as zfs;
use zen_core::header::FsHeader as CoreHeader;
use zen_core::keys::FsKeys as CoreKeys;
use zen_core::keyslot::{self, Argon2Params, Unlock};
use zen_core::rng::OsRng;
use zen_core::seal;
use zen_core::sig;
use zen_core::{kdf, labels, opaque, pwkey, token};
use zeroize::Zeroizing;

type R<T> = Result<T, JsValue>;

fn arr<const N: usize>(b: &[u8]) -> R<[u8; N]> {
    b.try_into().map_err(|_| js_err("param"))
}

fn to_js<T: Serialize>(v: &T) -> R<JsValue> {
    v.serialize(&serializer())
        .map_err(|e| js_err(&format!("wire: {e}")))
}

fn from_js<T: for<'de> Deserialize<'de>>(v: JsValue) -> R<T> {
    serde_wasm_bindgen::from_value(v).map_err(|e| js_err(&format!("wire: {e}")))
}

fn byte_list(v: Vec<js_sys::Uint8Array>) -> Vec<Vec<u8>> {
    v.into_iter().map(|a| a.to_vec()).collect()
}

fn ids(v: Vec<js_sys::Uint8Array>) -> R<Vec<zfs::Id>> {
    v.into_iter().map(|a| arr(&a.to_vec())).collect()
}

fn argon(m_cost_kib: u32, t_cost: u32, p_cost: u32) -> Argon2Params {
    Argon2Params {
        m_cost_kib,
        t_cost,
        p_cost,
    }
}

#[wasm_bindgen(typescript_custom_section)]
const TS_TYPES: &str = r#"
/** Plaintext node meta (spec/formats.md §11.2). */
export interface NodeMeta { type: "dir" | "file" | "symlink"; name: string; mode: number; mtimeMs: number; xattrs?: Uint8Array }
/** Plaintext file manifest (spec/formats.md §11.3). */
export interface Manifest { size: number; chunkSize: number; chunks: Uint8Array[] }
/** Plaintext event body (spec/formats.md §5). */
export interface EventBody { sender: Uint8Array; hlc: bigint; causation: Uint8Array; payload: Uint8Array }
/** The kind and key epoch in a sealed object's header. */
export interface Peek { kind: number; epoch: number }
/** A keyslot's type and id. */
export interface SlotInfo { slotType: number; slotId: Uint8Array }
/** A new recovery-key slot and its key, shown to the user once. */
export interface RecoverySlot { slot: Uint8Array; recoveryKey: Uint8Array }
/** A WebAuthn PRF slot's credential id and PRF salt. */
export interface PrfParams { credentialId: Uint8Array; prfSalt: Uint8Array }
/** The end of an OPAQUE exchange: the message to send and the export key. */
export interface OpaqueFinished { message: Uint8Array; exportKey: Uint8Array; serverPublicKey: Uint8Array }
/** A verified device certificate. */
export interface DeviceCert { devicePublic: Uint8Array; createdUnix: bigint }
"#;

// ------------------------------------------------------------------ misc

/// Random bytes from the platform RNG.
#[wasm_bindgen(js_name = randomBytes)]
pub fn random_bytes(n: usize) -> R<Vec<u8>> {
    let mut out = vec![0; n];
    zen_core::rng::Rng::fill(&mut OsRng, &mut out).map_err(core_err)?;
    Ok(out)
}

/// The domain-separation label of a signature purpose or other constant, by
/// its zen-core name (for example `SIG_ACL`).
#[wasm_bindgen]
pub fn label(name: &str) -> R<String> {
    Ok(match name {
        "SIG_DEVICE_CERT" => labels::SIG_DEVICE_CERT,
        "SIG_COMMIT" => labels::SIG_COMMIT,
        "SIG_ACL" => labels::SIG_ACL,
        "SIG_EVENT" => labels::SIG_EVENT,
        "SIG_SESSION" => labels::SIG_SESSION,
        "SIG_PASSWORD_SESSION" => labels::SIG_PASSWORD_SESSION,
        _ => return Err(js_err("param")),
    }
    .to_string())
}

/// `H(doc)` of a signed ACL (formats.md §9.2).
#[wasm_bindgen(js_name = aclHash)]
pub fn acl_hash(doc: &[u8]) -> Vec<u8> {
    kdf::acl_hash(doc).to_vec()
}

/// `FP(bytes)`: the fingerprint of encoded public key material.
#[wasm_bindgen]
pub fn fingerprint(public_bytes: &[u8]) -> Vec<u8> {
    kdf::fingerprint(public_bytes).to_vec()
}

/// The message a session signature covers: `lp(challenge) ‖ lp(origin)`.
#[wasm_bindgen(js_name = sessionMessage)]
pub fn session_message(challenge: &[u8], origin: &str) -> Vec<u8> {
    zen_proto::session_message(challenge, origin)
}

/// A login name normalized as the server does, or `undefined` if invalid.
#[wasm_bindgen(js_name = normalizeLogin)]
pub fn normalize_login(name: &str) -> Option<String> {
    zen_proto::normalize_login(name)
}

/// Whether `origin` is a valid `scheme://host[:port]` (auth.md §5).
#[wasm_bindgen(js_name = validOrigin)]
pub fn valid_origin(origin: &str) -> bool {
    zen_proto::valid_origin(origin)
}

/// A passkey's credential-store id from its WebAuthn `rawId`.
#[wasm_bindgen(js_name = passkeyCredentialId)]
pub fn passkey_credential_id(raw_id: &[u8]) -> Vec<u8> {
    keyslot::passkey_credential_id(raw_id).to_vec()
}

/// The kind and key epoch of a sealed object.
#[wasm_bindgen(js_name = peekSealed, unchecked_return_type = "Peek")]
pub fn peek_sealed(sealed: &[u8]) -> R<JsValue> {
    let (kind, epoch) = seal::peek(sealed).map_err(core_err)?;
    let o = js_sys::Object::new();
    js_sys::Reflect::set(&o, &"kind".into(), &(kind as u8).into())?;
    js_sys::Reflect::set(&o, &"epoch".into(), &epoch.into())?;
    Ok(o.into())
}

/// Incremental `expect_ranges` hash (api.md §6).
#[wasm_bindgen]
pub struct RangeHasher(kdf::RangeHasher);

#[wasm_bindgen]
impl RangeHasher {
    /// An empty hash.
    #[wasm_bindgen(constructor)]
    pub fn new() -> RangeHasher {
        RangeHasher(kdf::RangeHasher::new())
    }

    /// Add one `(stored key, version)` item, in range order.
    pub fn update(&mut self, stored_key: &[u8], version: &[u8]) -> R<()> {
        self.0.update(stored_key, &arr(version)?);
        Ok(())
    }

    /// The hash so far.
    pub fn finalize(&self) -> Vec<u8> {
        self.0.finalize().to_vec()
    }
}

impl Default for RangeHasher {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------------ fs keys

/// The keys of one fs at one epoch. Secret: free it when done.
#[wasm_bindgen]
pub struct FsKeys(pub(crate) CoreKeys);

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsEvent {
    #[serde(with = "serde_bytes")]
    sender: Vec<u8>,
    hlc: u64,
    #[serde(with = "serde_bytes")]
    causation: Vec<u8>,
    #[serde(with = "serde_bytes")]
    payload: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsMeta {
    #[serde(rename = "type")]
    node_type: String,
    name: String,
    mode: u32,
    mtime_ms: f64,
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Vec::is_empty")]
    xattrs: Vec<u8>,
}

impl JsMeta {
    fn to_core(&self) -> R<zfs::NodeMeta> {
        Ok(zfs::NodeMeta {
            node_type: match self.node_type.as_str() {
                "dir" => zfs::NodeType::Dir,
                "file" => zfs::NodeType::File,
                "symlink" => zfs::NodeType::Symlink,
                _ => return Err(js_err("param")),
            },
            name: self.name.clone(),
            mode: self.mode,
            mtime_ms: self.mtime_ms as u64,
            xattrs: self.xattrs.clone(),
        })
    }

    fn from_core(m: zfs::NodeMeta) -> Self {
        JsMeta {
            node_type: match m.node_type {
                zfs::NodeType::Dir => "dir",
                zfs::NodeType::File => "file",
                zfs::NodeType::Symlink => "symlink",
            }
            .into(),
            name: m.name,
            mode: m.mode,
            mtime_ms: m.mtime_ms as f64,
            xattrs: m.xattrs,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsManifest {
    size: f64,
    chunk_size: u32,
    chunks: Vec<serde_bytes::ByteBuf>,
}

fn key_token(t: Option<Vec<u8>>) -> R<Option<[u8; 16]>> {
    t.map(|t| arr(&t)).transpose()
}

#[wasm_bindgen]
impl FsKeys {
    /// New keys for fs `fs_id` at epoch 0.
    pub fn generate(fs_id: u32) -> R<FsKeys> {
        Ok(FsKeys(
            CoreKeys::generate(fs_id, &mut OsRng).map_err(core_err)?,
        ))
    }

    /// Keys from a 72-byte bundle (a keyslot payload).
    #[wasm_bindgen(js_name = fromBundle)]
    pub fn from_bundle(bundle: &[u8]) -> R<FsKeys> {
        Ok(FsKeys(CoreKeys::from_bundle(bundle).map_err(core_err)?))
    }

    /// The 72-byte bundle. Secret.
    #[wasm_bindgen(js_name = toBundle)]
    pub fn to_bundle(&self) -> Vec<u8> {
        self.0.to_bundle().to_vec()
    }

    /// The fs id.
    #[wasm_bindgen(getter, js_name = fsId)]
    pub fn fs_id(&self) -> u32 {
        self.0.fs_id
    }

    /// The key epoch.
    #[wasm_bindgen(getter)]
    pub fn epoch(&self) -> u32 {
        self.0.epoch
    }

    /// The keys of the previous epoch, from its chain record.
    pub fn previous(&self, record: &[u8]) -> R<FsKeys> {
        Ok(FsKeys(self.0.previous(record).map_err(core_err)?))
    }

    /// The stored KV key of a path of elements (formats.md §3.2). A prefix
    /// of a path gives the prefix of its stored key.
    #[wasm_bindgen(js_name = kvKey)]
    pub fn kv_key(&self, elements: Vec<js_sys::Uint8Array>) -> Vec<u8> {
        token::kv_key(&self.0, &byte_list(elements))
    }

    /// Seal a KV value under its stored key.
    #[wasm_bindgen(js_name = sealValue)]
    pub fn seal_value(&self, stored_key: &[u8], plaintext: &[u8]) -> R<Vec<u8>> {
        seal::seal_value(&self.0, stored_key, plaintext, &mut OsRng).map_err(core_err)
    }

    /// Open a KV value. The value's epoch must be this key's.
    #[wasm_bindgen(js_name = openValue)]
    pub fn open_value(&self, stored_key: &[u8], sealed: &[u8]) -> R<Vec<u8>> {
        seal::open_value(&self.0, stored_key, sealed).map_err(core_err)
    }

    /// The keys of a topic path (formats.md §3.3).
    pub fn topic(&self, segments: Vec<js_sys::Uint8Array>) -> TopicKeys {
        TopicKeys(token::TopicKeys::new(&self.0, &byte_list(segments)))
    }

    /// Seal an event body for a topic.
    #[wasm_bindgen(js_name = sealEvent)]
    pub fn seal_event(
        &self,
        topic: &TopicKeys,
        key_token: Option<Vec<u8>>,
        #[wasm_bindgen(unchecked_param_type = "EventBody")] body: JsValue,
    ) -> R<Vec<u8>> {
        let b: JsEvent = from_js(body)?;
        let body = seal::EventBody {
            sender: arr(&b.sender)?,
            hlc: b.hlc,
            causation: b.causation,
            payload: b.payload,
        };
        let token = self::key_token(key_token)?;
        seal::seal_event(&self.0, &topic.0, token.as_ref(), &body, &mut OsRng).map_err(core_err)
    }

    /// Open an event of a topic.
    #[wasm_bindgen(js_name = openEvent, unchecked_return_type = "EventBody")]
    pub fn open_event(
        &self,
        topic: &TopicKeys,
        key_token: Option<Vec<u8>>,
        sealed: &[u8],
    ) -> R<JsValue> {
        let token = self::key_token(key_token)?;
        let b = seal::open_event(&self.0, &topic.0, token.as_ref(), sealed).map_err(core_err)?;
        to_js(&JsEvent {
            sender: b.sender.to_vec(),
            hlc: b.hlc,
            causation: b.causation,
            payload: b.payload,
        })
    }

    /// Seal a node's meta.
    #[wasm_bindgen(js_name = sealMeta)]
    pub fn seal_meta(
        &self,
        tree: &[u8],
        node: &[u8],
        #[wasm_bindgen(unchecked_param_type = "NodeMeta")] meta: JsValue,
    ) -> R<Vec<u8>> {
        let m: JsMeta = from_js(meta)?;
        zfs::seal_meta(&self.0, &arr(tree)?, &arr(node)?, &m.to_core()?, &mut OsRng)
            .map_err(core_err)
    }

    /// Open a node's meta.
    #[wasm_bindgen(js_name = openMeta, unchecked_return_type = "NodeMeta")]
    pub fn open_meta(&self, tree: &[u8], node: &[u8], sealed: &[u8]) -> R<JsValue> {
        let m = zfs::open_meta(&self.0, &arr(tree)?, &arr(node)?, sealed).map_err(core_err)?;
        to_js(&JsMeta::from_core(m))
    }

    /// Seal a file version's manifest.
    #[wasm_bindgen(js_name = sealManifest)]
    pub fn seal_manifest(
        &self,
        tree: &[u8],
        node: &[u8],
        #[wasm_bindgen(unchecked_param_type = "Manifest")] manifest: JsValue,
    ) -> R<Vec<u8>> {
        let m: JsManifest = from_js(manifest)?;
        let m = zfs::Manifest {
            size: m.size as u64,
            chunk_size: m.chunk_size,
            chunks: m
                .chunks
                .iter()
                .map(|c| arr(c))
                .collect::<R<Vec<zfs::Id>>>()?,
        };
        zfs::seal_manifest(&self.0, &arr(tree)?, &arr(node)?, &m, &mut OsRng).map_err(core_err)
    }

    /// Open a file version's manifest.
    #[wasm_bindgen(js_name = openManifest, unchecked_return_type = "Manifest")]
    pub fn open_manifest(&self, tree: &[u8], node: &[u8], sealed: &[u8]) -> R<JsValue> {
        let m = zfs::open_manifest(&self.0, &arr(tree)?, &arr(node)?, sealed).map_err(core_err)?;
        to_js(&JsManifest {
            size: m.size as f64,
            chunk_size: m.chunk_size,
            chunks: m
                .chunks
                .iter()
                .map(|c| serde_bytes::ByteBuf::from(c.to_vec()))
                .collect(),
        })
    }

    /// Seal a chunk.
    #[wasm_bindgen(js_name = sealChunk)]
    pub fn seal_chunk(&self, chunk_id: &[u8], data: &[u8]) -> R<Vec<u8>> {
        zfs::seal_chunk(&self.0, &arr(chunk_id)?, data, &mut OsRng).map_err(core_err)
    }

    /// Open a chunk.
    #[wasm_bindgen(js_name = openChunk)]
    pub fn open_chunk(&self, chunk_id: &[u8], sealed: &[u8]) -> R<Vec<u8>> {
        zfs::open_chunk(&self.0, &arr(chunk_id)?, sealed).map_err(core_err)
    }
}

/// The keys of one topic. Secret.
#[wasm_bindgen]
pub struct TopicKeys(pub(crate) token::TopicKeys);

#[wasm_bindgen]
impl TopicKeys {
    /// The 16·n-byte topic id the server sees.
    #[wasm_bindgen(getter)]
    pub fn id(&self) -> Vec<u8> {
        self.0.id().to_vec()
    }

    /// A child topic.
    pub fn child(&self, segment: &[u8]) -> TopicKeys {
        TopicKeys(self.0.child(segment))
    }

    /// The 16-byte key token of an event key.
    #[wasm_bindgen(js_name = eventKeyToken)]
    pub fn event_key_token(&self, key: &[u8]) -> Vec<u8> {
        self.0.event_key_token(key).to_vec()
    }
}

// ------------------------------------------------------------------ keyslots

/// A passphrase keyslot (type 1).
#[wasm_bindgen(js_name = createPassphraseSlot)]
pub fn create_passphrase_slot(
    keys: &FsKeys,
    passphrase: &[u8],
    m_cost_kib: u32,
    t_cost: u32,
    p_cost: u32,
) -> R<Vec<u8>> {
    keyslot::create_passphrase(
        &keys.0,
        passphrase,
        argon(m_cost_kib, t_cost, p_cost),
        &mut OsRng,
    )
    .map_err(core_err)
}

/// A recovery-key keyslot (type 2) and its new key.
#[wasm_bindgen(js_name = createRecoverySlot, unchecked_return_type = "RecoverySlot")]
pub fn create_recovery_slot(keys: &FsKeys) -> R<JsValue> {
    let (slot, key) = keyslot::create_recovery(&keys.0, &mut OsRng).map_err(core_err)?;
    let o = js_sys::Object::new();
    js_sys::Reflect::set(&o, &"slot".into(), &js_sys::Uint8Array::from(&slot[..]))?;
    js_sys::Reflect::set(
        &o,
        &"recoveryKey".into(),
        &js_sys::Uint8Array::from(&key[..]),
    )?;
    Ok(o.into())
}

/// A device keyslot (type 3) for an encoded device public key.
#[wasm_bindgen(js_name = createDeviceSlot)]
pub fn create_device_slot(keys: &FsKeys, device_public: &[u8]) -> R<Vec<u8>> {
    let dev = sig::DevicePublic::decode(device_public).map_err(core_err)?;
    keyslot::create_device(&keys.0, &dev, &mut OsRng).map_err(core_err)
}

/// A WebAuthn PRF keyslot (type 4).
#[wasm_bindgen(js_name = createPrfSlot)]
pub fn create_prf_slot(
    keys: &FsKeys,
    credential_id: &[u8],
    prf_salt: &[u8],
    prf_output: &[u8],
) -> R<Vec<u8>> {
    let out = Zeroizing::new(arr::<32>(prf_output)?);
    keyslot::create_webauthn_prf(
        &keys.0,
        &arr(credential_id)?,
        &arr(prf_salt)?,
        &out,
        &mut OsRng,
    )
    .map_err(core_err)
}

/// An OPAQUE export-key keyslot (type 5).
#[wasm_bindgen(js_name = createOpaqueSlot)]
pub fn create_opaque_slot(keys: &FsKeys, credential_id: &[u8], export_key: &[u8]) -> R<Vec<u8>> {
    let k = Zeroizing::new(arr::<{ keyslot::OPAQUE_EXPORT_KEY_LEN }>(export_key)?);
    keyslot::create_opaque_export(&keys.0, &arr(credential_id)?, &k, &mut OsRng).map_err(core_err)
}

/// A keyslot's type and id (any type).
#[wasm_bindgen(js_name = slotInfo, unchecked_return_type = "SlotInfo")]
pub fn slot_info(slot: &[u8]) -> R<JsValue> {
    let i = keyslot::slot_info(slot).map_err(core_err)?;
    let o = js_sys::Object::new();
    js_sys::Reflect::set(&o, &"slotType".into(), &i.slot_type.into())?;
    js_sys::Reflect::set(
        &o,
        &"slotId".into(),
        &js_sys::Uint8Array::from(&i.slot_id[..]),
    )?;
    Ok(o.into())
}

/// The credential id and PRF salt of a type-4 slot.
#[wasm_bindgen(js_name = prfSlotParams, unchecked_return_type = "PrfParams")]
pub fn prf_slot_params(slot: &[u8]) -> R<JsValue> {
    let (cred, salt) = keyslot::webauthn_prf_params(slot).map_err(core_err)?;
    let o = js_sys::Object::new();
    js_sys::Reflect::set(
        &o,
        &"credentialId".into(),
        &js_sys::Uint8Array::from(&cred[..]),
    )?;
    js_sys::Reflect::set(&o, &"prfSalt".into(), &js_sys::Uint8Array::from(&salt[..]))?;
    Ok(o.into())
}

/// The credential id of a type-5 slot.
#[wasm_bindgen(js_name = opaqueSlotCredential)]
pub fn opaque_slot_credential(slot: &[u8]) -> R<Vec<u8>> {
    Ok(keyslot::opaque_export_credential(slot)
        .map_err(core_err)?
        .to_vec())
}

/// The recipient fingerprint of a type-3 slot.
#[wasm_bindgen(js_name = deviceSlotRecipient)]
pub fn device_slot_recipient(slot: &[u8]) -> R<Vec<u8>> {
    Ok(keyslot::device_recipient(slot).map_err(core_err)?.to_vec())
}

/// Open a passphrase slot.
#[wasm_bindgen(js_name = openPassphraseSlot)]
pub fn open_passphrase_slot(slot: &[u8], passphrase: &[u8]) -> R<FsKeys> {
    Ok(FsKeys(
        keyslot::open(slot, Unlock::Passphrase(passphrase)).map_err(core_err)?,
    ))
}

/// Open a recovery-key slot.
#[wasm_bindgen(js_name = openRecoverySlot)]
pub fn open_recovery_slot(slot: &[u8], recovery_key: &[u8]) -> R<FsKeys> {
    let k = Zeroizing::new(arr::<32>(recovery_key)?);
    Ok(FsKeys(
        keyslot::open(slot, Unlock::Recovery(&k)).map_err(core_err)?,
    ))
}

/// Open a device slot with the device's secret.
#[wasm_bindgen(js_name = openDeviceSlot)]
pub fn open_device_slot(slot: &[u8], device: &DeviceSecret) -> R<FsKeys> {
    Ok(FsKeys(
        keyslot::open(slot, Unlock::Device(&device.0)).map_err(core_err)?,
    ))
}

/// Open a WebAuthn PRF slot with the authenticator's PRF output.
#[wasm_bindgen(js_name = openPrfSlot)]
pub fn open_prf_slot(slot: &[u8], prf_output: &[u8]) -> R<FsKeys> {
    let out = Zeroizing::new(arr::<32>(prf_output)?);
    Ok(FsKeys(
        keyslot::open(slot, Unlock::WebAuthnPrf(&out)).map_err(core_err)?,
    ))
}

/// Open an OPAQUE export-key slot with the export key of a sign-in.
#[wasm_bindgen(js_name = openOpaqueSlot)]
pub fn open_opaque_slot(slot: &[u8], export_key: &[u8]) -> R<FsKeys> {
    let k = Zeroizing::new(arr::<{ keyslot::OPAQUE_EXPORT_KEY_LEN }>(export_key)?);
    Ok(FsKeys(
        keyslot::open(slot, Unlock::OpaqueExport(&k)).map_err(core_err)?,
    ))
}

// ------------------------------------------------------------------ header

/// A decoded fs header (formats.md §12). Not secret.
#[wasm_bindgen]
pub struct FsHeader(CoreHeader);

#[wasm_bindgen]
impl FsHeader {
    /// A header for new keys at epoch 0, with no slots.
    #[wasm_bindgen(js_name = create)]
    pub fn create(keys: &FsKeys) -> R<FsHeader> {
        Ok(FsHeader(CoreHeader::new(&keys.0).map_err(core_err)?))
    }

    /// Decode and check a header.
    pub fn decode(bytes: &[u8]) -> R<FsHeader> {
        Ok(FsHeader(CoreHeader::decode(bytes).map_err(core_err)?))
    }

    /// Encode.
    pub fn encode(&self) -> R<Vec<u8>> {
        self.0.encode().map_err(core_err)
    }

    /// The fs id.
    #[wasm_bindgen(getter, js_name = fsId)]
    pub fn fs_id(&self) -> u32 {
        self.0.fs_id
    }

    /// The epoch new data is sealed under.
    #[wasm_bindgen(getter, js_name = currentEpoch)]
    pub fn current_epoch(&self) -> u32 {
        self.0.current_epoch
    }

    /// The keyslots, in order.
    pub fn slots(&self) -> Vec<js_sys::Uint8Array> {
        self.0
            .slots
            .iter()
            .map(|s| js_sys::Uint8Array::from(&s[..]))
            .collect()
    }

    /// Add a keyslot.
    #[wasm_bindgen(js_name = addSlot)]
    pub fn add_slot(&mut self, slot: &[u8]) -> R<()> {
        self.0.add_slot(slot.to_vec()).map_err(core_err)
    }

    /// Remove the keyslot with this id; whether one was removed.
    #[wasm_bindgen(js_name = removeSlot)]
    pub fn remove_slot(&mut self, slot_id: &[u8]) -> R<bool> {
        Ok(self.0.remove_slot(&arr(slot_id)?))
    }

    /// Start a new epoch; returns its keys. Existing slots still wrap the old
    /// epoch: re-wrap or remove them.
    pub fn rotate(&mut self, keys: &FsKeys) -> R<FsKeys> {
        Ok(FsKeys(
            self.0.rotate(&keys.0, &mut OsRng).map_err(core_err)?,
        ))
    }

    /// The keys of an earlier epoch, walking the chain back from `keys`.
    #[wasm_bindgen(js_name = keysAt)]
    pub fn keys_at(&self, keys: &FsKeys, epoch: u32) -> R<FsKeys> {
        Ok(FsKeys(self.0.keys_at(&keys.0, epoch).map_err(core_err)?))
    }
}

// ------------------------------------------------------------------ identities

/// A hybrid signing identity (a user). Secret.
#[wasm_bindgen]
pub struct SigningIdentity(sig::SigningIdentity);

#[wasm_bindgen]
impl SigningIdentity {
    /// The identity of a 32-byte seed.
    #[wasm_bindgen(js_name = fromSeed)]
    pub fn from_seed(seed: &[u8]) -> R<SigningIdentity> {
        let s = Zeroizing::new(arr::<32>(seed)?);
        Ok(SigningIdentity(sig::SigningIdentity::from_seed(&s)))
    }

    /// The encoded public identity (formats.md §7.2).
    #[wasm_bindgen(getter, js_name = publicIdentity)]
    pub fn public_identity(&self) -> Vec<u8> {
        self.0.public().encode()
    }

    /// The user fingerprint.
    #[wasm_bindgen(getter)]
    pub fn fingerprint(&self) -> Vec<u8> {
        self.0.public().fingerprint().to_vec()
    }

    /// A hybrid signature with a purpose label (see [`label`]).
    pub fn sign(&self, purpose: &str, msg: &[u8]) -> R<Vec<u8>> {
        self.0.sign(purpose, msg).map_err(core_err)
    }

    /// Certify a device's public key (formats.md §7.4).
    #[wasm_bindgen(js_name = issueDeviceCert)]
    pub fn issue_device_cert(&self, device_public: &[u8], created_unix: u64) -> R<Vec<u8>> {
        let dev = sig::DevicePublic::decode(device_public).map_err(core_err)?;
        sig::issue_device_cert(&self.0, &dev, created_unix).map_err(core_err)
    }
}

/// Verify a hybrid signature by an encoded public identity.
#[wasm_bindgen(js_name = verifySignature)]
pub fn verify_signature(
    public_identity: &[u8],
    purpose: &str,
    msg: &[u8],
    signature: &[u8],
) -> R<()> {
    sig::PublicIdentity::decode(public_identity)
        .map_err(core_err)?
        .verify(purpose, msg, signature)
        .map_err(core_err)
}

/// Verify a device certificate against its user's public identity.
#[wasm_bindgen(js_name = verifyDeviceCert, unchecked_return_type = "DeviceCert")]
pub fn verify_device_cert(user_public: &[u8], cert: &[u8]) -> R<JsValue> {
    let user = sig::PublicIdentity::decode(user_public).map_err(core_err)?;
    let (dev, created) = sig::verify_device_cert(&user, cert).map_err(core_err)?;
    let o = js_sys::Object::new();
    js_sys::Reflect::set(
        &o,
        &"devicePublic".into(),
        &js_sys::Uint8Array::from(&dev.encode()[..]),
    )?;
    js_sys::Reflect::set(&o, &"createdUnix".into(), &js_sys::BigInt::from(created))?;
    Ok(o.into())
}

/// A device's secret: its signing key and X-Wing key. Secret.
#[wasm_bindgen]
pub struct DeviceSecret(sig::DeviceSecret);

#[wasm_bindgen]
impl DeviceSecret {
    /// The device of a 32-byte seed.
    #[wasm_bindgen(js_name = fromSeed)]
    pub fn from_seed(seed: &[u8]) -> R<DeviceSecret> {
        let s = Zeroizing::new(arr::<32>(seed)?);
        Ok(DeviceSecret(sig::DeviceSecret::from_seed(&s)))
    }

    /// The encoded device public key (identity and X-Wing key).
    #[wasm_bindgen(getter, js_name = devicePublic)]
    pub fn device_public(&self) -> Vec<u8> {
        self.0.public().encode()
    }

    /// The device fingerprint.
    #[wasm_bindgen(getter)]
    pub fn fingerprint(&self) -> Vec<u8> {
        self.0.public().fingerprint().to_vec()
    }

    /// Sign a session challenge for `origin` (sign-in method 1).
    #[wasm_bindgen(js_name = signSession)]
    pub fn sign_session(&self, challenge: &[u8], origin: &str) -> R<Vec<u8>> {
        self.0
            .signing()
            .sign(
                labels::SIG_SESSION,
                &zen_proto::session_message(challenge, origin),
            )
            .map_err(core_err)
    }
}

/// 32 random bytes for a new identity or device seed. Secret.
#[wasm_bindgen(js_name = newSeed)]
pub fn new_seed() -> R<Vec<u8>> {
    random_bytes(32)
}

// ------------------------------------------------------------------ password keys

/// A password-derived signing key (sign-in method 6). Secret.
#[wasm_bindgen]
pub struct PasswordKey {
    key: pwkey::PasswordKey,
    salt: Option<[u8; pwkey::SALT_LEN]>,
}

#[wasm_bindgen]
impl PasswordKey {
    /// Registration: a fresh salt (read it with `salt`), creation floors.
    pub fn create(password: &[u8], m_cost_kib: u32, t_cost: u32, p_cost: u32) -> R<PasswordKey> {
        let (key, salt) =
            pwkey::PasswordKey::create(password, argon(m_cost_kib, t_cost, p_cost), &mut OsRng)
                .map_err(core_err)?;
        Ok(PasswordKey {
            key,
            salt: Some(salt),
        })
    }

    /// Sign-in: derive from the salt and parameters the server returned.
    pub fn derive(
        password: &[u8],
        salt: &[u8],
        m_cost_kib: u32,
        t_cost: u32,
        p_cost: u32,
    ) -> R<PasswordKey> {
        let salt = arr(salt)?;
        let key = pwkey::PasswordKey::derive(password, &salt, argon(m_cost_kib, t_cost, p_cost))
            .map_err(core_err)?;
        Ok(PasswordKey {
            key,
            salt: Some(salt),
        })
    }

    /// The salt.
    #[wasm_bindgen(getter)]
    pub fn salt(&self) -> Option<Vec<u8>> {
        self.salt.map(|s| s.to_vec())
    }

    /// The public identity the server stores.
    #[wasm_bindgen(getter, js_name = publicIdentity)]
    pub fn public_identity(&self) -> Vec<u8> {
        self.key.public().encode()
    }

    /// Sign a session challenge for `origin`.
    #[wasm_bindgen(js_name = signSession)]
    pub fn sign_session(&self, challenge: &[u8], origin: &str) -> R<Vec<u8>> {
        self.key.sign_session(challenge, origin).map_err(core_err)
    }
}

// ------------------------------------------------------------------ OPAQUE

fn finished(f: opaque::Finished) -> R<JsValue> {
    let o = js_sys::Object::new();
    js_sys::Reflect::set(
        &o,
        &"message".into(),
        &js_sys::Uint8Array::from(&f.message[..]),
    )?;
    js_sys::Reflect::set(
        &o,
        &"exportKey".into(),
        &js_sys::Uint8Array::from(&f.export_key[..]),
    )?;
    js_sys::Reflect::set(
        &o,
        &"serverPublicKey".into(),
        &js_sys::Uint8Array::from(&f.server_public_key[..]),
    )?;
    Ok(o.into())
}

/// An OPAQUE registration in progress (sign-in method 3).
#[wasm_bindgen]
pub struct OpaqueRegistration {
    state: Option<opaque::Registration>,
    request: Vec<u8>,
}

#[wasm_bindgen]
impl OpaqueRegistration {
    /// Start: read the message to send with `request`.
    pub fn start(password: &[u8]) -> R<OpaqueRegistration> {
        let (state, request) =
            opaque::Registration::start(password, &mut OsRng).map_err(core_err)?;
        Ok(OpaqueRegistration {
            state: Some(state),
            request,
        })
    }

    /// The registration request.
    #[wasm_bindgen(getter)]
    pub fn request(&self) -> Vec<u8> {
        self.request.clone()
    }

    /// Finish with the server's response: the upload and the export key.
    #[wasm_bindgen(unchecked_return_type = "OpaqueFinished")]
    pub fn finish(
        &mut self,
        password: &[u8],
        response: &[u8],
        m_cost_kib: u32,
        t_cost: u32,
        p_cost: u32,
    ) -> R<JsValue> {
        let state = self.state.take().ok_or_else(|| js_err("param"))?;
        finished(
            state
                .finish(
                    password,
                    response,
                    argon(m_cost_kib, t_cost, p_cost),
                    &mut OsRng,
                )
                .map_err(core_err)?,
        )
    }
}

/// An OPAQUE sign-in in progress (sign-in method 3).
#[wasm_bindgen]
pub struct OpaqueLogin {
    state: Option<opaque::Login>,
    request: Vec<u8>,
}

#[wasm_bindgen]
impl OpaqueLogin {
    /// Start: read the message to send with `request`.
    pub fn start(password: &[u8]) -> R<OpaqueLogin> {
        let (state, request) = opaque::Login::start(password, &mut OsRng).map_err(core_err)?;
        Ok(OpaqueLogin {
            state: Some(state),
            request,
        })
    }

    /// The credential request.
    #[wasm_bindgen(getter)]
    pub fn request(&self) -> Vec<u8> {
        self.request.clone()
    }

    /// Finish with the server's response, bound to `origin`: the
    /// finalization and the export key.
    #[wasm_bindgen(unchecked_return_type = "OpaqueFinished")]
    pub fn finish(
        &mut self,
        password: &[u8],
        response: &[u8],
        m_cost_kib: u32,
        t_cost: u32,
        p_cost: u32,
        origin: &str,
    ) -> R<JsValue> {
        let state = self.state.take().ok_or_else(|| js_err("param"))?;
        finished(
            state
                .finish(
                    password,
                    response,
                    argon(m_cost_kib, t_cost, p_cost),
                    origin,
                    &mut OsRng,
                )
                .map_err(core_err)?,
        )
    }
}

// ------------------------------------------------------------------ filesystem ops

/// A hybrid logical clock (formats.md §11.1).
#[wasm_bindgen]
pub struct Clock(zfs::Clock);

#[wasm_bindgen]
impl Clock {
    /// A clock that has seen `last` (0 for a new one).
    #[wasm_bindgen(constructor)]
    pub fn new(last: u64) -> Clock {
        Clock(zfs::Clock { last })
    }

    /// The last timestamp issued or observed.
    #[wasm_bindgen(getter)]
    pub fn last(&self) -> u64 {
        self.0.last
    }

    /// A new timestamp, after everything seen, from the wall clock in ms.
    pub fn tick(&mut self, wall_ms: f64) -> u64 {
        self.0.tick(wall_ms as u64)
    }

    /// Observe a timestamp from elsewhere.
    pub fn observe(&mut self, seen: u64) {
        self.0.observe(seen)
    }
}

/// `unix_ms << 16 | counter`.
#[wasm_bindgen]
pub fn hlc(unix_ms: f64, counter: u16) -> u64 {
    zfs::hlc(unix_ms as u64, counter)
}

/// The milliseconds of an HLC.
#[wasm_bindgen(js_name = hlcMs)]
pub fn hlc_ms(hlc: u64) -> f64 {
    zfs::hlc_ms(hlc) as f64
}

/// The canonical bytes of a `move` op (formats.md §11.5).
#[wasm_bindgen(js_name = moveOpBytes)]
pub fn move_op_bytes(
    fs: u32,
    tree: &[u8],
    node: &[u8],
    parent: &[u8],
    hlc: u64,
    meta: &[u8],
) -> R<Vec<u8>> {
    Ok(zfs::move_bytes(
        fs,
        &arr(tree)?,
        &arr(node)?,
        &arr(parent)?,
        hlc,
        meta,
    ))
}

/// The canonical bytes of a `meta` op.
#[wasm_bindgen(js_name = metaOpBytes)]
pub fn meta_op_bytes(fs: u32, tree: &[u8], node: &[u8], hlc: u64, meta: &[u8]) -> R<Vec<u8>> {
    Ok(zfs::meta_bytes(fs, &arr(tree)?, &arr(node)?, hlc, meta))
}

/// The canonical bytes of a `write` op.
#[wasm_bindgen(js_name = writeOpBytes)]
pub fn write_op_bytes(
    fs: u32,
    tree: &[u8],
    node: &[u8],
    replaces: Vec<js_sys::Uint8Array>,
    chunks: Vec<js_sys::Uint8Array>,
    manifest: &[u8],
) -> R<Vec<u8>> {
    let replaces: Vec<[u8; zfs::DOT_LEN]> = replaces
        .into_iter()
        .map(|d| arr(&d.to_vec()))
        .collect::<R<_>>()?;
    Ok(zfs::write_bytes(
        fs,
        &arr(tree)?,
        &arr(node)?,
        &replaces,
        &ids(chunks)?,
        manifest,
    ))
}

/// The next op-chain value: `chain_next(prev, op, device_fp)`.
#[wasm_bindgen(js_name = chainNext)]
pub fn chain_next(prev: &[u8], op_bytes: &[u8], device_fp: &[u8]) -> R<Vec<u8>> {
    Ok(zfs::chain_next(&arr(prev)?, op_bytes, &arr(device_fp)?).to_vec())
}
