//! Milestone 6 integrity for JS: device checkpoints (formats.md §14), signed
//! fs headers (§12.1), event chains (§5.1) and tree states (fs.md §9.1).

use crate::crypto::{FsKeys, JsEvent, R, arr, from_js, to_js};
use crate::{core_err, js_err};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use wasm_bindgen::prelude::*;
use zen_core::fs as zfs;
use zen_core::header::{self, SignedHeader};
use zen_core::integrity::{self as ck, CheckpointBody, Entry, Head, Seen, TopicChain};
use zen_core::rng::OsRng;
use zen_core::{seal, sig};

#[wasm_bindgen(typescript_custom_section)]
const TS_TYPES: &str = r#"
/** A `(version, hash)` pair in a checkpoint: the ACL or signed header a device verified. */
export interface CheckpointHead { version: bigint; hash: Uint8Array }
/** One of a device's own event chains on a topic (formats.md §5.1). */
export interface CheckpointChain { chainId: Uint8Array; dseq: bigint; dhash: Uint8Array }
/** An object entry of a checkpoint (formats.md §14.1). */
export type CheckpointEntry =
  | { type: "topic"; topic: Uint8Array; head: Uint8Array; chains: CheckpointChain[] }
  | { type: "tree"; tree: Uint8Array; ops: bigint; chain: Uint8Array; state: Uint8Array }
  | { type: "db"; ns: Uint8Array; seq: bigint; hash: Uint8Array }
  | { type: "vol"; vol: Uint8Array; seq: bigint; hash: Uint8Array }
  | { type: "svc"; svc: Uint8Array; conn: Uint8Array; inAcked: bigint; outHead: bigint };
/** Another device's head a checkpoint names. */
export interface CheckpointSeen { device: Uint8Array; seq: bigint; hash: Uint8Array }
/** A device checkpoint body (formats.md §14.1). `objects` and `seen` are sorted on encode. */
export interface CheckpointBody { fs: number; device: Uint8Array; seq: bigint; prev: Uint8Array; hlc: bigint; acl: CheckpointHead; header: CheckpointHead; objects: CheckpointEntry[]; seen: CheckpointSeen[] }
/** A decoded signed fs header (formats.md §12.1) and its hash `HH`. */
export interface SignedHeaderInfo { fs: number; version: bigint; prev: Uint8Array; signer: Uint8Array; header: Uint8Array; hash: Uint8Array }
/** A node state as the change feed delivers it, the input of a tree-state leaf (fs.md §9.1). */
export interface StateNode { node: Uint8Array; parent?: Uint8Array; moveHlc: bigint; moveDevice: Uint8Array; meta?: Uint8Array; metaHlc?: bigint; metaDevice?: Uint8Array; versions: number; changed: Uint8Array }
"#;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsHead {
    version: u64,
    hash: ByteBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsChain {
    chain_id: ByteBuf,
    dseq: u64,
    dhash: ByteBuf,
}

/// An entry as a flat object: `type` says which fields are present.
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct JsEntry {
    #[serde(rename = "type")]
    ty: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    topic: Option<ByteBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    head: Option<ByteBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chains: Option<Vec<JsChain>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tree: Option<ByteBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ops: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chain: Option<ByteBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<ByteBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ns: Option<ByteBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vol: Option<ByteBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hash: Option<ByteBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    svc: Option<ByteBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    conn: Option<ByteBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    in_acked: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    out_head: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsSeen {
    device: ByteBuf,
    seq: u64,
    hash: ByteBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsBody {
    fs: u32,
    device: ByteBuf,
    seq: u64,
    prev: ByteBuf,
    hlc: u64,
    acl: JsHead,
    header: JsHead,
    objects: Vec<JsEntry>,
    seen: Vec<JsSeen>,
}

fn need<T>(v: Option<T>) -> R<T> {
    v.ok_or_else(|| js_err("param"))
}

fn buf(b: &[u8]) -> Option<ByteBuf> {
    Some(ByteBuf::from(b.to_vec()))
}

impl JsEntry {
    fn into_core(self) -> R<Entry> {
        Ok(match self.ty.as_str() {
            "topic" => Entry::Topic {
                topic: need(self.topic)?.into_vec(),
                head: arr(&need(self.head)?)?,
                chains: need(self.chains)?
                    .into_iter()
                    .map(|c| {
                        Ok(TopicChain {
                            chain_id: arr(&c.chain_id)?,
                            dseq: c.dseq,
                            dhash: arr(&c.dhash)?,
                        })
                    })
                    .collect::<R<_>>()?,
            },
            "tree" => Entry::Tree {
                tree: arr(&need(self.tree)?)?,
                ops: need(self.ops)?,
                chain: arr(&need(self.chain)?)?,
                state: arr(&need(self.state)?)?,
            },
            "db" => Entry::Db {
                ns: need(self.ns)?.into_vec(),
                seq: need(self.seq)?,
                hash: arr(&need(self.hash)?)?,
            },
            "vol" => Entry::Vol {
                vol: need(self.vol)?.into_vec(),
                seq: need(self.seq)?,
                hash: arr(&need(self.hash)?)?,
            },
            "svc" => Entry::Svc {
                svc: need(self.svc)?.into_vec(),
                conn: arr(&need(self.conn)?)?,
                in_acked: need(self.in_acked)?,
                out_head: need(self.out_head)?,
            },
            _ => return Err(js_err("param")),
        })
    }

    fn from_core(e: Entry) -> Self {
        match e {
            Entry::Topic {
                topic,
                head,
                chains,
            } => JsEntry {
                ty: "topic".into(),
                topic: buf(&topic),
                head: buf(&head),
                chains: Some(
                    chains
                        .into_iter()
                        .map(|c| JsChain {
                            chain_id: ByteBuf::from(c.chain_id.to_vec()),
                            dseq: c.dseq,
                            dhash: ByteBuf::from(c.dhash.to_vec()),
                        })
                        .collect(),
                ),
                ..Default::default()
            },
            Entry::Tree {
                tree,
                ops,
                chain,
                state,
            } => JsEntry {
                ty: "tree".into(),
                tree: buf(&tree),
                ops: Some(ops),
                chain: buf(&chain),
                state: buf(&state),
                ..Default::default()
            },
            Entry::Db { ns, seq, hash } => JsEntry {
                ty: "db".into(),
                ns: buf(&ns),
                seq: Some(seq),
                hash: buf(&hash),
                ..Default::default()
            },
            Entry::Vol { vol, seq, hash } => JsEntry {
                ty: "vol".into(),
                vol: buf(&vol),
                seq: Some(seq),
                hash: buf(&hash),
                ..Default::default()
            },
            Entry::Svc {
                svc,
                conn,
                in_acked,
                out_head,
            } => JsEntry {
                ty: "svc".into(),
                svc: buf(&svc),
                conn: buf(&conn),
                in_acked: Some(in_acked),
                out_head: Some(out_head),
                ..Default::default()
            },
        }
    }
}

/// A body from its JS form, sorted into canonical order.
pub(crate) fn body_from_js(v: JsValue) -> R<CheckpointBody> {
    let b: JsBody = from_js(v)?;
    let head = |h: JsHead| -> R<Head> {
        Ok(Head {
            version: h.version,
            hash: arr(&h.hash)?,
        })
    };
    let mut body = CheckpointBody {
        fs: b.fs,
        device: arr(&b.device)?,
        seq: b.seq,
        prev: arr(&b.prev)?,
        hlc: b.hlc,
        acl: head(b.acl)?,
        header: head(b.header)?,
        objects: b
            .objects
            .into_iter()
            .map(JsEntry::into_core)
            .collect::<R<_>>()?,
        seen: b
            .seen
            .into_iter()
            .map(|s| {
                Ok(Seen {
                    device: arr(&s.device)?,
                    seq: s.seq,
                    hash: arr(&s.hash)?,
                })
            })
            .collect::<R<_>>()?,
    };
    body.sort();
    Ok(body)
}

fn body_to_js(b: CheckpointBody) -> R<JsValue> {
    let head = |h: Head| JsHead {
        version: h.version,
        hash: ByteBuf::from(h.hash.to_vec()),
    };
    to_js(&JsBody {
        fs: b.fs,
        device: ByteBuf::from(b.device.to_vec()),
        seq: b.seq,
        prev: ByteBuf::from(b.prev.to_vec()),
        hlc: b.hlc,
        acl: head(b.acl),
        header: head(b.header),
        objects: b.objects.into_iter().map(JsEntry::from_core).collect(),
        seen: b
            .seen
            .into_iter()
            .map(|s| JsSeen {
                device: ByteBuf::from(s.device.to_vec()),
                seq: s.seq,
                hash: ByteBuf::from(s.hash.to_vec()),
            })
            .collect(),
    })
}

// ------------------------------------------------------------- checkpoints

/// Encode a checkpoint body (formats.md §14.1), sorting `objects` and `seen`.
#[wasm_bindgen(js_name = encodeCheckpoint)]
pub fn encode_checkpoint(
    #[wasm_bindgen(unchecked_param_type = "CheckpointBody")] body: JsValue,
) -> R<Vec<u8>> {
    body_from_js(body)?.encode().map_err(core_err)
}

/// Decode a checkpoint body, refusing anything but its canonical encoding.
#[wasm_bindgen(js_name = decodeCheckpoint, unchecked_return_type = "CheckpointBody")]
pub fn decode_checkpoint(bytes: &[u8]) -> R<JsValue> {
    body_to_js(CheckpointBody::decode(bytes).map_err(core_err)?)
}

/// `HC(body)` of encoded body bytes.
#[wasm_bindgen(js_name = checkpointHash)]
pub fn checkpoint_hash(body: &[u8]) -> Vec<u8> {
    ck::checkpoint_hash(body).to_vec()
}

/// The body bytes of a signed checkpoint, unverified (for `checkpointHash`).
#[wasm_bindgen(js_name = checkpointBodyBytes)]
pub fn checkpoint_body_bytes(signed: &[u8]) -> R<Vec<u8>> {
    Ok(ck::split_signed(signed).map_err(core_err)?.0.to_vec())
}

/// Verify a signed checkpoint with the device's encoded public key (from a
/// certificate the caller verified), and return its body.
#[wasm_bindgen(js_name = verifyCheckpoint, unchecked_return_type = "CheckpointBody")]
pub fn verify_checkpoint(device_public: &[u8], signed: &[u8]) -> R<JsValue> {
    let dev = sig::DevicePublic::decode(device_public).map_err(core_err)?;
    body_to_js(ck::verify_checkpoint(&dev, signed).map_err(core_err)?)
}

#[wasm_bindgen]
impl FsKeys {
    /// Seal a signed checkpoint (kind 11) of `device` at `seq`.
    #[wasm_bindgen(js_name = sealCheckpoint)]
    pub fn seal_checkpoint(&self, device: &[u8], seq: u64, signed: &[u8]) -> R<Vec<u8>> {
        ck::seal_checkpoint(&self.0, &arr(device)?, seq, signed, &mut OsRng).map_err(core_err)
    }

    /// Open a sealed checkpoint of `device` at `seq`: the signed bytes. The
    /// checkpoint's epoch must be this key's.
    #[wasm_bindgen(js_name = openCheckpoint)]
    pub fn open_checkpoint(&self, device: &[u8], seq: u64, sealed: &[u8]) -> R<Vec<u8>> {
        ck::open_checkpoint(&self.0, &arr(device)?, seq, sealed).map_err(core_err)
    }
}

// ----------------------------------------------------------- signed header

/// Whether stored header bytes are the signed form (formats.md §12.1).
#[wasm_bindgen(js_name = isSignedHeader)]
pub fn is_signed_header(bytes: &[u8]) -> bool {
    header::is_signed(bytes)
}

/// Decode a signed header (no signature check), with its hash `HH`.
#[wasm_bindgen(js_name = decodeSignedHeader, unchecked_return_type = "SignedHeaderInfo")]
pub fn decode_signed_header(bytes: &[u8]) -> R<JsValue> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Info {
        fs: u32,
        version: u64,
        prev: ByteBuf,
        signer: ByteBuf,
        header: ByteBuf,
        hash: ByteBuf,
    }
    let h = SignedHeader::decode(bytes).map_err(core_err)?;
    to_js(&Info {
        fs: h.fs,
        version: h.version,
        prev: ByteBuf::from(h.prev.to_vec()),
        signer: ByteBuf::from(h.signer.to_vec()),
        hash: ByteBuf::from(h.hash().to_vec()),
        header: ByteBuf::from(h.header),
    })
}

/// Verify a signed header's signature with the signer's encoded public
/// identity, which must be the identity it names.
#[wasm_bindgen(js_name = verifySignedHeader)]
pub fn verify_signed_header(bytes: &[u8], signer_public: &[u8]) -> R<()> {
    let id = sig::PublicIdentity::decode(signer_public).map_err(core_err)?;
    SignedHeader::decode(bytes)
        .map_err(core_err)?
        .verify(&id)
        .map_err(core_err)
}

/// The acceptance rules of formats.md §12.1 for `next` over `current` (either
/// form, or `undefined`), with the admins' encoded public identities.
#[wasm_bindgen(js_name = validateHeaderSuccessor)]
pub fn validate_header_successor(
    fs: u32,
    current: Option<Vec<u8>>,
    next: &[u8],
    admins: Vec<js_sys::Uint8Array>,
) -> R<()> {
    let admins = admins
        .into_iter()
        .map(|a| sig::PublicIdentity::decode(&a.to_vec()).map_err(core_err))
        .collect::<R<Vec<_>>>()?;
    header::validate_header_successor(fs, current.as_deref(), next, |fp| {
        admins.iter().find(|a| a.fingerprint() == *fp).cloned()
    })
    .map_err(core_err)
}

// ------------------------------------------------------------ event chains

/// `HE(body)` of an event body (formats.md §5.1): the `dprev` of the chain's
/// next event.
#[wasm_bindgen(js_name = eventChainHash)]
pub fn event_chain_hash(
    #[wasm_bindgen(unchecked_param_type = "EventBody")] body: JsValue,
) -> R<Vec<u8>> {
    let b = from_js::<JsEvent>(body)?.to_core()?;
    Ok(seal::event_chain_hash(&b.encode()).to_vec())
}

/// The plaintext bytes of an event body (formats.md §5).
#[wasm_bindgen(js_name = encodeEventBody)]
pub fn encode_event_body(
    #[wasm_bindgen(unchecked_param_type = "EventBody")] body: JsValue,
) -> R<Vec<u8>> {
    Ok(from_js::<JsEvent>(body)?.to_core()?.encode())
}

/// Decode event body bytes (version 1 or 2).
#[wasm_bindgen(js_name = decodeEventBody, unchecked_return_type = "EventBody")]
pub fn decode_event_body(bytes: &[u8]) -> R<JsValue> {
    to_js(&JsEvent::from_core(
        seal::EventBody::decode(bytes).map_err(core_err)?,
    ))
}

// -------------------------------------------------------------- tree state

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsStateNode {
    node: ByteBuf,
    #[serde(default)]
    parent: Option<ByteBuf>,
    move_hlc: u64,
    move_device: ByteBuf,
    #[serde(default)]
    meta: Option<ByteBuf>,
    #[serde(default)]
    meta_hlc: Option<u64>,
    #[serde(default)]
    meta_device: Option<ByteBuf>,
    versions: u32,
    changed: ByteBuf,
}

fn state_node(v: JsValue) -> R<zfs::StateNode> {
    let n: JsStateNode = from_js(v)?;
    let meta = match n.meta {
        Some(m) => Some((
            m.into_vec(),
            n.meta_hlc.unwrap_or(0),
            match &n.meta_device {
                Some(d) => arr(d)?,
                None => [0; 32],
            },
        )),
        None => None,
    };
    Ok(zfs::StateNode {
        node: arr(&n.node)?,
        parent: n.parent.map(|p| arr(&p)).transpose()?,
        move_hlc: n.move_hlc,
        move_device: arr(&n.move_device)?,
        meta,
        versions: n.versions,
        changed: arr(&n.changed)?,
    })
}

/// A node's leaf in the tree state (fs.md §9.1).
#[wasm_bindgen(js_name = stateLeaf)]
pub fn state_leaf(#[wasm_bindgen(unchecked_param_type = "StateNode")] node: JsValue) -> R<Vec<u8>> {
    Ok(zfs::state_leaf(&state_node(node)?).to_vec())
}

/// A tree's state, kept incrementally as nodes change (fs.md §9.1).
#[wasm_bindgen]
pub struct TreeState(zfs::TreeState);

impl Default for TreeState {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl TreeState {
    /// An empty tree.
    #[wasm_bindgen(constructor)]
    pub fn new() -> TreeState {
        TreeState(zfs::TreeState::new())
    }

    /// Set a node from its state (computes its leaf).
    pub fn set(
        &mut self,
        #[wasm_bindgen(unchecked_param_type = "StateNode")] node: JsValue,
    ) -> R<()> {
        let n = state_node(node)?;
        self.0.set(n.node, zfs::state_leaf(&n));
        Ok(())
    }

    /// Remove a node (a tombstone).
    pub fn remove(&mut self, node: &[u8]) -> R<()> {
        self.0.remove(&arr(node)?);
        Ok(())
    }

    /// The number of nodes.
    #[wasm_bindgen(getter)]
    pub fn size(&self) -> usize {
        self.0.len()
    }

    /// The tree state.
    pub fn root(&mut self) -> Vec<u8> {
        self.0.root().to_vec()
    }
}
