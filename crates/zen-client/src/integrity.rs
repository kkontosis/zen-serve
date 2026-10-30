//! Integrity of one fs (spec/formats.md §14): this device's checkpoints,
//! the pins of every device's verified head and of each object, and the
//! checks of server answers against them. A port of the TS client's
//! `integrity.ts`, with the same rules, so a TS device and a Rust device
//! verify each other.

use crate::acl::AclVersion;
use crate::device::DeviceKey;
use crate::error::{Alarm, AlarmKind, AlarmObject, Error, Result};
use crate::fs::KeyRing;
use crate::pins::{DeviceRecord, PinsFile};
use crate::session::Session;
use crate::short;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zen_core::integrity::{
    CheckpointBody, Entry, ExportedPins, Head, Seen, TopicChain, checkpoint_hash, open_checkpoint,
    seal_checkpoint, sign_checkpoint, split_signed, verify_checkpoint,
};
use zen_core::rng::OsRng;
use zen_core::sig::{DevicePublic, PublicIdentity, SIGNATURE_LEN, verify_device_cert};
use zen_proto::{
    ByteBuf, CheckpointGet, CheckpointHeadList, CheckpointHeads, CheckpointList, CheckpointPut,
    CheckpointVersion, DeviceList, DevicesGet,
};

/// Bytes a signed, sealed checkpoint adds to its body: lp, signature, seal (§14.2).
const OVERHEAD: usize = 4 + SIGNATURE_LEN + 128;
/// Own chains carried per topic entry.
const MAX_CHAINS: usize = 16;
const PUT_ATTEMPTS: u32 = 6;

/// Called with each alarm, before it is returned.
pub type AlarmHook = Arc<dyn Fn(&Alarm) + Send + Sync>;

/// Options of [`crate::Fs::integrity`].
#[derive(Clone)]
pub struct IntegrityOptions {
    /// The signer. Without one, the client verifies only and writes no checkpoints.
    pub signer: Option<DeviceKey>,
    /// Where pins are kept across processes. Default: memory only.
    pub pins: Option<PathBuf>,
    /// Checkpoint after this many changes (default 64; 1: every change).
    pub sig_every: u32,
    /// …or this long after the last change (default 5 s).
    pub delay: Duration,
    /// Called with each alarm.
    pub on_alarm: Option<AlarmHook>,
}

impl Default for IntegrityOptions {
    fn default() -> Self {
        IntegrityOptions {
            signer: None,
            pins: None,
            sig_every: 64,
            delay: Duration::from_secs(5),
            on_alarm: None,
        }
    }
}

/// An entry's identity: its type and id bytes (svc: service and connection).
type EntryKey = (u8, Vec<u8>, Vec<u8>);

fn key_of(e: &Entry) -> EntryKey {
    let (t, a, b) = e.sort_key();
    (t, a.to_vec(), b.to_vec())
}

fn object_of(e: &Entry) -> AlarmObject {
    match e {
        Entry::Topic { .. } => AlarmObject::Topic,
        Entry::Tree { .. } => AlarmObject::Tree,
        Entry::Db { .. } => AlarmObject::Db,
        Entry::Vol { .. } => AlarmObject::Vol,
        Entry::Svc { .. } => AlarmObject::Svc,
    }
}

fn id_of(e: &Entry) -> Vec<u8> {
    match e {
        Entry::Topic { topic, .. } => topic.clone(),
        Entry::Tree { tree, .. } => tree.to_vec(),
        Entry::Db { ns, .. } => ns.clone(),
        Entry::Vol { vol, .. } => vol.clone(),
        Entry::Svc { svc, conn, .. } => [svc.as_slice(), conn].concat(),
    }
}

fn alarm(
    kind: AlarmKind,
    object: AlarmObject,
    id: Option<Vec<u8>>,
    detail: impl Into<String>,
) -> Alarm {
    Alarm {
        kind,
        object,
        id,
        detail: detail.into(),
    }
}

fn entry_alarm(kind: AlarmKind, e: &Entry, detail: &str) -> Alarm {
    alarm(kind, object_of(e), Some(id_of(e)), detail)
}

/// One writer's newer entry over its older one.
fn merge_entry(a: Option<&Entry>, b: &Entry) -> Entry {
    let Some(a) = a.filter(|a| a.type_id() == b.type_id()) else {
        return b.clone();
    };
    match (a, b) {
        (
            Entry::Topic {
                head: ha,
                chains: ca,
                ..
            },
            Entry::Topic {
                topic,
                head: hb,
                chains: cb,
            },
        ) => {
            let mut chains: Vec<TopicChain> = Vec::new();
            for c in ca.iter().chain(cb) {
                match chains.iter().position(|x| x.chain_id == c.chain_id) {
                    Some(i) if c.dseq >= chains[i].dseq => {
                        chains.remove(i);
                        chains.push(c.clone());
                    }
                    Some(_) => {}
                    None => chains.push(c.clone()),
                }
            }
            let skip = chains.len().saturating_sub(MAX_CHAINS);
            Entry::Topic {
                topic: topic.clone(),
                head: (*ha).max(*hb),
                chains: chains.split_off(skip),
            }
        }
        (Entry::Tree { ops: oa, .. }, Entry::Tree { ops: ob, .. }) => {
            if oa > ob {
                a.clone()
            } else {
                b.clone()
            }
        }
        (Entry::Db { seq: sa, .. }, Entry::Db { seq: sb, .. })
        | (Entry::Vol { seq: sa, .. }, Entry::Vol { seq: sb, .. }) => {
            if sa > sb {
                a.clone()
            } else {
                b.clone()
            }
        }
        (
            Entry::Svc {
                in_acked: ia,
                out_head: oa,
                ..
            },
            Entry::Svc {
                svc,
                conn,
                in_acked: ib,
                out_head: ob,
            },
        ) => Entry::Svc {
            svc: svc.clone(),
            conn: *conn,
            in_acked: (*ia).max(*ib),
            out_head: (*oa).max(*ob),
        },
        _ => b.clone(),
    }
}

/// Why `next` (a writer's newer entry) is lower than `prev`, if it is.
fn lower_than(prev: &Entry, next: &Entry) -> Option<&'static str> {
    match (prev, next) {
        (
            Entry::Topic {
                head: hp,
                chains: cp,
                ..
            },
            Entry::Topic {
                head: hn,
                chains: cn,
                ..
            },
        ) => {
            if hn < hp {
                return Some("topic head went back");
            }
            for c in cp {
                let Some(n) = cn.iter().find(|x| x.chain_id == c.chain_id) else {
                    continue;
                };
                if n.dseq < c.dseq {
                    return Some("chain went back");
                }
                if n.dseq == c.dseq && n.dhash != c.dhash {
                    return Some("chain rewritten");
                }
            }
            None
        }
        (
            Entry::Tree {
                ops: op,
                chain: chp,
                state: sp,
                ..
            },
            Entry::Tree {
                ops: on,
                chain: chn,
                state: sn,
                ..
            },
        ) => {
            if on < op {
                Some("tree ops went back")
            } else if on == op && (chn != chp || sn != sp) {
                Some("tree state changed at the same ops")
            } else {
                None
            }
        }
        (
            Entry::Db {
                seq: sp, hash: hp, ..
            },
            Entry::Db {
                seq: sn, hash: hn, ..
            },
        )
        | (
            Entry::Vol {
                seq: sp, hash: hp, ..
            },
            Entry::Vol {
                seq: sn, hash: hn, ..
            },
        ) => {
            if sn < sp {
                Some("root seq went back")
            } else if sn == sp && hn != hp {
                Some("root changed at the same seq")
            } else {
                None
            }
        }
        (
            Entry::Svc {
                in_acked: ip,
                out_head: op,
                ..
            },
            Entry::Svc {
                in_acked: inn,
                out_head: on,
                ..
            },
        ) => (inn < ip || on < op).then_some("cursors went back"),
        _ => None,
    }
}

/// Why two entries at one position (tree ops, root seq) differ, if they do.
fn divergent(a: &Entry, b: &Entry) -> Option<&'static str> {
    match (a, b) {
        (
            Entry::Tree {
                ops: oa,
                chain: ca,
                state: sa,
                ..
            },
            Entry::Tree {
                ops: ob,
                chain: cb,
                state: sb,
                ..
            },
        ) if oa == ob && (ca != cb || sa != sb) => Some("tree state changed at the same ops"),
        (
            Entry::Db {
                seq: sa, hash: ha, ..
            },
            Entry::Db {
                seq: sb, hash: hb, ..
            },
        )
        | (
            Entry::Vol {
                seq: sa, hash: ha, ..
            },
            Entry::Vol {
                seq: sb, hash: hb, ..
            },
        ) if sa == sb && ha != hb => Some("root changed at the same seq"),
        _ => None,
    }
}

/// A pinned device head: its newest verified checkpoint.
#[derive(Debug, Clone)]
pub struct DevicePin {
    /// Its seq.
    pub seq: u64,
    /// `HC` of its body.
    pub hash: [u8; 32],
    /// The body.
    pub body: CheckpointBody,
}

/// The floor of a topic: the newest head, and each writer's chain position.
#[derive(Debug, Clone, Default)]
struct TopicPin {
    head: Option<[u8; 12]>,
    /// By (writer, chain id): (dseq, dhash).
    chains: HashMap<([u8; 32], [u8; 12]), (u64, [u8; 32])>,
}

#[derive(Default)]
struct State {
    alarms: Vec<Alarm>,
    dev_pins: BTreeMap<[u8; 32], DevicePin>,
    acl_pin: Option<Head>,
    acl_bytes: Option<Vec<u8>>,
    header_pin: Option<Head>,
    topics: HashMap<Vec<u8>, TopicPin>,
    floors: HashMap<EntryKey, Entry>,
    pending: BTreeMap<EntryKey, Entry>,
    changed_at: HashMap<EntryKey, u64>,
    tick: u64,
    changes: u32,
    /// Verified `(device, seq)` hashes beyond the pins.
    known: HashMap<([u8; 32], u64), [u8; 32]>,
    /// Device public keys, for the ACL version they were found in.
    dev_keys: HashMap<[u8; 32], DevicePublic>,
    dev_keys_acl: Option<u64>,
    acl_hashes: HashMap<u64, [u8; 32]>,
    closed: bool,
    untrusted: bool,
    header_stale: bool,
    /// The cadence timer's generation: bumping it cancels the armed one.
    timer: u64,
}

impl State {
    /// Raise the floors with a body of `writer` (spec/formats.md §14.4 rule 4).
    fn raise_floor(&mut self, writer: [u8; 32], e: &Entry) -> std::result::Result<(), Alarm> {
        if let Entry::Topic {
            topic,
            head,
            chains,
        } = e
        {
            let t = self.topics.entry(topic.clone()).or_default();
            if t.head.is_none_or(|h| *head > h) {
                t.head = Some(*head);
            }
            for c in chains {
                let k = (writer, c.chain_id);
                if t.chains.get(&k).is_none_or(|cur| c.dseq > cur.0) {
                    t.chains.insert(k, (c.dseq, c.dhash));
                }
            }
            return Ok(());
        }
        let k = key_of(e);
        let cur = self.floors.get(&k);
        // Two writers at one position with different states: two histories.
        if let Some(why) = cur.and_then(|c| divergent(c, e)) {
            return Err(entry_alarm(AlarmKind::Fork, e, why));
        }
        let merged = merge_entry(cur, e);
        self.floors.insert(k, merged);
        Ok(())
    }

    fn raise_floors(&mut self, body: &CheckpointBody) -> std::result::Result<(), Alarm> {
        for e in &body.objects {
            self.raise_floor(body.device, e)?;
        }
        Ok(())
    }

    fn pins_file(&self, fs: u32) -> PinsFile {
        PinsFile {
            fs,
            acl: self.acl_bytes.clone(),
            header: self.header_pin,
            devices: self
                .dev_pins
                .iter()
                .map(|(fp, p)| DeviceRecord {
                    device: *fp,
                    seq: p.seq,
                    hash: p.hash,
                    body: p.body.encode().expect("a verified body encodes"),
                })
                .collect(),
        }
    }
}

struct Inner {
    session: Session,
    fs: u32,
    opts: IntegrityOptions,
    me: Option<[u8; 32]>,
    state: Mutex<State>,
    serial: tokio::sync::Mutex<()>,
    ring: Mutex<Option<Arc<KeyRing>>>,
}

/// Integrity of one fs (cheap to clone: clones share it). Get it with
/// [`crate::Fs::integrity`].
#[derive(Clone)]
pub struct Integrity(Arc<Inner>);

impl Integrity {
    pub(crate) fn attach(session: Session, fs: u32, opts: IntegrityOptions) -> Result<Self> {
        let me = opts.signer.as_ref().map(DeviceKey::fp);
        let pins = match &opts.pins {
            Some(p) => PinsFile::load(p, fs)?,
            None => None,
        };
        let i = Integrity(Arc::new(Inner {
            session,
            fs,
            opts,
            me,
            state: Mutex::new(State::default()),
            serial: tokio::sync::Mutex::new(()),
            ring: Mutex::new(None),
        }));
        let acl = i.0.session.acl();
        let mut st = i.lock();
        if let Some(p) = pins {
            if let Some(a) = &p.acl {
                acl.pin(a)?;
            }
            st.header_pin = p.header;
            for d in p.devices {
                let body = CheckpointBody::decode(&d.body)
                    .map_err(|_| Error::format("pins file: a device body doesn't decode"))?;
                st.raise_floors(&body)
                    .map_err(|a| Error::format(format!("pins file: {a}")))?;
                st.dev_pins.insert(
                    d.device,
                    DevicePin {
                        seq: d.seq,
                        hash: d.hash,
                        body,
                    },
                );
            }
        }
        if let Some(v) = acl.pinned() {
            st.acl_pin = Some(Head {
                version: v.version,
                hash: v.hash(),
            });
            st.acl_bytes = Some(v.bytes);
        }
        drop(st);
        Ok(i)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().expect("integrity state")
    }

    /// The fs id.
    pub fn fs(&self) -> u32 {
        self.0.fs
    }

    /// The signer's device fingerprint, if any.
    pub fn device_fp(&self) -> Option<[u8; 32]> {
        self.0.me
    }

    /// Whether no alarm was raised: once one is, checkpoints stop.
    pub fn trusted(&self) -> bool {
        !self.lock().untrusted
    }

    /// Every alarm raised, oldest first.
    pub fn alarms(&self) -> Vec<Alarm> {
        self.lock().alarms.clone()
    }

    /// A device's pinned head (this device's: its newest written).
    pub fn device_head(&self, device: &[u8; 32]) -> Option<DevicePin> {
        self.lock().dev_pins.get(device).cloned()
    }

    /// The pinned header `(version, HH)`, if a signed header was seen.
    pub fn header_pin(&self) -> Option<Head> {
        self.lock().header_pin
    }

    /// The pinned ACL head.
    pub fn acl_pin(&self) -> Option<Head> {
        self.lock().acl_pin
    }

    /// The newest topic head verified or written.
    pub fn topic_head(&self, topic: &[u8]) -> Option<[u8; 12]> {
        self.lock().topics.get(topic).and_then(|t| t.head)
    }

    /// The floor of an object (tree, db, vol, svc): the newest entry
    /// verified or written for the object `e` names.
    pub fn floor(&self, e: &Entry) -> Option<Entry> {
        self.lock().floors.get(&key_of(e)).cloned()
    }

    pub(crate) fn bind(&self, ring: Arc<KeyRing>) {
        *self.0.ring.lock().expect("integrity ring") = Some(ring);
    }

    fn ring(&self) -> Result<Arc<KeyRing>> {
        self.0
            .ring
            .lock()
            .expect("integrity ring")
            .clone()
            .ok_or_else(|| Error::local("locked", "unlock the fs before syncing checkpoints"))
    }

    // ---------------------------------------------------------------- alarms

    /// Record an alarm (the fs is untrusted from now on) and return it as an error.
    pub fn raise(&self, a: Alarm) -> Error {
        {
            let mut st = self.lock();
            st.untrusted = true;
            st.timer += 1;
            st.alarms.push(a.clone());
        }
        if let Some(f) = &self.0.opts.on_alarm {
            f(&a);
        }
        Error::Integrity(a)
    }

    // ---------------------------------------------------------------- pins

    fn persist(&self, st: &State) -> Result<()> {
        match &self.0.opts.pins {
            Some(p) => st.pins_file(self.0.fs).save(p),
            None => Ok(()),
        }
    }

    fn keep_device(&self, fp: [u8; 32], pin: DevicePin) -> Result<()> {
        let mut st = self.lock();
        st.known.insert((fp, pin.seq), pin.hash);
        st.dev_pins.insert(fp, pin);
        self.persist(&st)
    }

    fn keep_acl(&self, v: &AclVersion) -> Result<()> {
        let hash = v.hash();
        let mut st = self.lock();
        st.acl_hashes.insert(v.version, hash);
        if st.acl_pin.is_some_and(|p| p.version >= v.version) {
            return Ok(());
        }
        if st.acl_pin.is_some() {
            self.changed(&mut st);
        }
        st.acl_pin = Some(Head {
            version: v.version,
            hash,
        });
        st.acl_bytes = Some(v.bytes.clone());
        self.persist(&st)
    }

    fn keep_header(&self, st: &mut State, h: Head) -> Result<()> {
        st.header_pin = Some(h);
        self.changed(st);
        self.persist(st)
    }

    /// Check a header the server served (formats.md §12.1, clients):
    /// `signed` is its `(version, HH)`, `None` for an unsigned one (or none).
    /// Raises the pin, or returns a rollback or fork.
    pub fn check_header(&self, signed: Option<Head>) -> Result<()> {
        let found = {
            let mut st = self.lock();
            match (st.header_pin, signed) {
                (None, None) => return Ok(()),
                (None, Some(s)) => return self.keep_header(&mut st, s),
                (Some(_), None) => Some("an unsigned header after a signed one".to_string()),
                (Some(p), Some(s)) if s.version < p.version => Some(format!(
                    "header version {}, pinned {}",
                    s.version, p.version
                )),
                (Some(p), Some(s)) if s.version == p.version => {
                    if s.hash != p.hash {
                        let a = alarm(
                            AlarmKind::Fork,
                            AlarmObject::Header,
                            None,
                            format!("another header at version {}", p.version),
                        );
                        drop(st);
                        return Err(self.raise(a));
                    }
                    return Ok(());
                }
                (Some(_), Some(s)) => return self.keep_header(&mut st, s),
            }
        };
        let detail = found.expect("a rollback");
        Err(self.raise(alarm(
            AlarmKind::Rollback,
            AlarmObject::Header,
            None,
            detail,
        )))
    }

    // ---------------------------------------------------------------- writing

    /// Record an object entry this device wrote or verified. With `count`,
    /// it counts toward the cadence.
    pub fn note(&self, entry: Entry, count: bool) -> Result<()> {
        let mut st = self.lock();
        if let Err(a) = st.raise_floor(self.0.me.unwrap_or([0; 32]), &entry) {
            drop(st);
            return Err(self.raise(a));
        }
        if self.0.me.is_none() || st.closed {
            return Ok(());
        }
        let k = key_of(&entry);
        let merged = merge_entry(st.pending.get(&k), &entry);
        st.pending.insert(k.clone(), merged);
        st.tick += 1;
        let t = st.tick;
        st.changed_at.insert(k, t);
        if count {
            self.changed(&mut st);
        }
        Ok(())
    }

    /// Check a server answer about an object against its floor now
    /// (formats.md §14.4), then `note` it. See [`Integrity::observe_at`].
    pub fn observe(&self, entry: Entry, count: bool) -> Result<()> {
        let at = self.floor(&entry);
        self.observe_at(entry, at, count)
    }

    /// Check a server answer about an object against the floors, then `note`
    /// it: the same position (tree `ops`, root `seq`) with another state is
    /// a fork; below `floor_at`, the floor when the answer was asked for, a
    /// rollback (a checkpoint that arrived meanwhile may be newer than the
    /// answer honestly is). `count`: count toward the cadence, for what this
    /// device wrote.
    pub fn observe_at(&self, entry: Entry, floor_at: Option<Entry>, count: bool) -> Result<()> {
        if matches!(entry, Entry::Topic { .. }) {
            return Err(Error::local("param", "topics check their reads themselves"));
        }
        let cur = self.floor(&entry);
        let why = cur
            .as_ref()
            .and_then(|c| divergent(c, &entry))
            .or_else(|| floor_at.as_ref().and_then(|w| divergent(w, &entry)));
        if let Some(why) = why {
            return Err(self.raise(entry_alarm(AlarmKind::Fork, &entry, why)));
        }
        if let Some(low) = floor_at.as_ref().and_then(|w| lower_than(w, &entry)) {
            return Err(self.raise(entry_alarm(AlarmKind::Rollback, &entry, low)));
        }
        self.note(entry, count)
    }

    /// Count a change toward the cadence (formats.md §14.3) and schedule a checkpoint.
    fn changed(&self, st: &mut State) {
        if self.0.me.is_none() || st.closed {
            return;
        }
        st.changes += 1;
        if st.untrusted {
            return;
        }
        st.timer += 1;
        let generation = st.timer;
        let now = st.changes >= self.0.opts.sig_every;
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let me = self.clone();
        let delay = self.0.opts.delay;
        rt.spawn(async move {
            if !now {
                tokio::time::sleep(delay).await;
                if me.lock().timer != generation {
                    return;
                }
            }
            let _ = me.checkpoint().await;
        });
    }

    /// Write a checkpoint now, if this device has changes to sign. Returns
    /// its seq, or `None` when there was nothing to write (or no signer, or
    /// an alarm stopped checkpoints).
    pub async fn checkpoint(&self) -> Result<Option<u64>> {
        let _one = self.0.serial.lock().await;
        self.checkpoint_now(false).await
    }

    /// Write a checkpoint now even without changes (it carries the newest
    /// `seen` links and claims): `None` only without a signer or after an alarm.
    pub async fn checkpoint_always(&self) -> Result<Option<u64>> {
        let _one = self.0.serial.lock().await;
        self.checkpoint_now(true).await
    }

    async fn checkpoint_now(&self, force: bool) -> Result<Option<u64>> {
        let (Some(signer), Some(me)) = (&self.0.opts.signer, self.0.me) else {
            return Ok(None);
        };
        {
            let mut st = self.lock();
            if st.untrusted || (!force && st.pending.is_empty() && st.changes == 0) {
                return Ok(None);
            }
            st.timer += 1;
        }
        let ring = self.ring()?;
        let acl = match self.0.session.acl().pinned() {
            Some(v) => v,
            None => self.acl_head().await?,
        };
        self.keep_acl(&acl)?;
        for attempt in 1.. {
            let own = self.lock().dev_pins.get(&me).cloned();
            let seq = own.as_ref().map_or(0, |p| p.seq) + 1;
            let body = self.build_body(seq, own.as_ref());
            let signed = sign_checkpoint(&signer.device, &body)?;
            let sealed = seal_checkpoint(&*ring.seal_keys()?, &me, seq, &signed, &mut OsRng)?;
            match self.put(me, seq, sealed).await {
                Ok(()) => {}
                Err(e) if e.is("version_mismatch") && attempt < PUT_ATTEMPTS => {
                    // Another process of this device took the seq: adopt its
                    // head (verified) and merge ours into it (formats.md §14.3).
                    self.adopt_own_head(me).await?;
                    continue;
                }
                Err(e) => return Err(e),
            }
            let hash = checkpoint_hash(split_signed(&signed)?.0);
            self.keep_device(me, DevicePin { seq, hash, body })?;
            let mut st = self.lock();
            st.pending.clear();
            st.changes = 0;
            return Ok(Some(seq));
        }
        unreachable!()
    }

    fn build_body(&self, seq: u64, own: Option<&DevicePin>) -> CheckpointBody {
        let me = self.0.me.expect("a signer");
        let st = self.lock();
        let mut objects: BTreeMap<EntryKey, Entry> = own
            .map(|p| {
                p.body
                    .objects
                    .iter()
                    .map(|e| (key_of(e), e.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (k, e) in &st.pending {
            let merged = merge_entry(objects.get(k), e);
            objects.insert(k.clone(), merged);
        }
        let seen = st
            .dev_pins
            .iter()
            .filter(|(fp, _)| **fp != me)
            .map(|(fp, p)| Seen {
                device: *fp,
                seq: p.seq,
                hash: p.hash,
            })
            .collect();
        let mut body = CheckpointBody {
            fs: self.0.fs,
            device: me,
            seq,
            prev: own.map_or([0; 32], |p| p.hash),
            hlc: self.0.session.hlc(),
            acl: st.acl_pin.unwrap_or_default(),
            header: st.header_pin.unwrap_or_default(),
            objects: objects.values().cloned().collect(),
            seen,
        };
        body.sort();
        // Too big: leave out the entries unchanged for longest (formats.md §14.3).
        let budget =
            (self.0.session.info().limits.max_value_bytes as usize).saturating_sub(OVERHEAD);
        let size = |b: &CheckpointBody| b.encode().map_or(usize::MAX, |e| e.len());
        if size(&body) > budget {
            let mut order: Vec<EntryKey> = objects.keys().cloned().collect();
            order.sort_by_key(|k| st.changed_at.get(k).copied().unwrap_or(0));
            for k in order {
                objects.remove(&k);
                body.objects = objects.values().cloned().collect();
                body.sort();
                if size(&body) <= budget {
                    break;
                }
            }
        }
        body
    }

    /// Send a put, resending the same bytes while the outcome is unknown
    /// (idempotent, api.md §15.1).
    async fn put(&self, me: [u8; 32], seq: u64, checkpoint: Vec<u8>) -> Result<()> {
        let req = CheckpointPut {
            fs: self.0.fs,
            device: me.to_vec(),
            seq,
            checkpoint,
        };
        for i in 1.. {
            match self
                .0
                .session
                .call::<_, CheckpointVersion>("/v1/checkpoint/put", &req)
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) if (e.is("network") || e.is("commit_unknown")) && i < 4 => {
                    tokio::time::sleep(Duration::from_millis(50 << i)).await;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    async fn adopt_own_head(&self, me: [u8; 32]) -> Result<()> {
        let mut heads = self.heads(Some(vec![me])).await?;
        if let Some(seq) = heads.get(&me).copied() {
            self.advance(me, seq, &mut heads).await?;
        }
        Ok(())
    }

    /// Write a last checkpoint (if there are changes) and stop the cadence.
    pub async fn close(&self) -> Result<()> {
        let dirty = {
            let mut st = self.lock();
            st.timer += 1;
            !st.pending.is_empty() || st.changes > 0
        };
        if dirty {
            self.checkpoint().await?;
        }
        self.lock().closed = true;
        Ok(())
    }

    // ---------------------------------------------------------------- verifying

    fn keep(&self) -> u64 {
        match self.0.session.info().limits.checkpoint_keep {
            0 => 1024,
            k => u64::from(k),
        }
    }

    async fn heads(&self, devices: Option<Vec<[u8; 32]>>) -> Result<BTreeMap<[u8; 32], u64>> {
        let r: CheckpointHeadList = self
            .0
            .session
            .call(
                "/v1/checkpoint/heads",
                &CheckpointHeads {
                    fs: self.0.fs,
                    devices: devices.map(|d| d.iter().map(|x| ByteBuf::from(x.to_vec())).collect()),
                },
            )
            .await?;
        r.heads
            .into_iter()
            .map(|h| {
                let fp: [u8; 32] = h
                    .device
                    .try_into()
                    .map_err(|_| Error::format("a device id that isn't 32 bytes"))?;
                Ok((fp, h.seq))
            })
            .collect()
    }

    async fn fetch(&self, device: &[u8; 32], seq: u64) -> Result<Option<Vec<u8>>> {
        let r: CheckpointList = self
            .0
            .session
            .call(
                "/v1/checkpoint/get",
                &CheckpointGet {
                    fs: self.0.fs,
                    device: device.to_vec(),
                    from: seq,
                    to: Some(seq),
                    limit: Some(1),
                },
            )
            .await?;
        Ok(r.items
            .into_iter()
            .next()
            .filter(|it| it.seq == seq)
            .map(|it| it.checkpoint))
    }

    /// The verified ACL head, as an alarm when its chain breaks.
    async fn acl_head(&self) -> Result<AclVersion> {
        match self.0.session.acl().head().await {
            Ok(v) => {
                self.keep_acl(&v)?;
                Ok(v)
            }
            Err(e) if e.is("acl_chain") => {
                let detail = e.to_string();
                let kind = if detail.contains("below") {
                    AlarmKind::Rollback
                } else {
                    AlarmKind::Fork
                };
                Err(self.raise(alarm(kind, AlarmObject::Acl, None, detail)))
            }
            Err(e) => Err(e),
        }
    }

    /// A device's public keys: from the ACL's members, or the registry
    /// checked against them.
    async fn device_public(&self, fp: &[u8; 32], acl: &AclVersion) -> Result<Option<DevicePublic>> {
        {
            let mut st = self.lock();
            if st.dev_keys_acl != Some(acl.version) {
                st.dev_keys.clear();
                for m in &acl.doc.members {
                    let Ok(id) = PublicIdentity::decode(&m.identity) else {
                        continue;
                    };
                    for cert in &m.devices {
                        // A cert that doesn't verify names no device.
                        if let Ok((dp, _)) = verify_device_cert(&id, cert) {
                            st.dev_keys.insert(dp.fingerprint(), dp);
                        }
                    }
                }
                st.dev_keys_acl = Some(acl.version);
            }
            if let Some(dp) = st.dev_keys.get(fp) {
                return Ok(Some(dp.clone()));
            }
        }
        let r: DeviceList = self
            .0
            .session
            .call(
                "/v1/devices/get",
                &DevicesGet {
                    devices: vec![ByteBuf::from(fp.to_vec())],
                },
            )
            .await?;
        let Some(d) = r.devices.into_iter().find(|d| d.device == fp) else {
            return Ok(None);
        };
        let Some(m) = acl.member(&d.user) else {
            return Ok(None);
        };
        let Ok(id) = PublicIdentity::decode(&m.identity) else {
            return Ok(None);
        };
        match verify_device_cert(&id, &d.cert) {
            Ok((dp, _)) if dp.fingerprint() == *fp => {
                self.lock().dev_keys.insert(*fp, dp.clone());
                Ok(Some(dp))
            }
            _ => Ok(None),
        }
    }

    /// Open and verify a checkpoint (formats.md §14.4 rule 1).
    fn open(
        &self,
        fp: &[u8; 32],
        seq: u64,
        sealed: &[u8],
        dp: &DevicePublic,
    ) -> Result<(CheckpointBody, [u8; 32])> {
        let invalid = |detail: String| {
            self.raise(alarm(
                AlarmKind::Invalid,
                AlarmObject::Checkpoint,
                Some(fp.to_vec()),
                detail,
            ))
        };
        let keys = match self.ring()?.keys_for(sealed) {
            Ok(k) => k,
            Err(e) if e.is("epoch_ahead") => return Err(e),
            Err(_) => return Err(invalid(format!("checkpoint {seq} doesn't open"))),
        };
        let signed = open_checkpoint(&keys, fp, seq, sealed)
            .map_err(|_| invalid(format!("checkpoint {seq} doesn't open")))?;
        let body = verify_checkpoint(dp, &signed)
            .map_err(|_| invalid(format!("checkpoint {seq} doesn't verify")))?;
        if body.fs != self.0.fs || body.device != *fp || body.seq != seq {
            return Err(invalid(format!("checkpoint {seq} names another place")));
        }
        Ok((body, checkpoint_hash(split_signed(&signed)?.0)))
    }

    /// Verify and pin device `fp`'s checkpoint at `seq` (rules 1, 2 and 4).
    /// Returns the new body, or `None` when nothing changed or the device
    /// isn't a member's.
    async fn advance(
        &self,
        fp: [u8; 32],
        seq: u64,
        heads: &mut BTreeMap<[u8; 32], u64>,
    ) -> Result<Option<CheckpointBody>> {
        let pin = self.lock().dev_pins.get(&fp).cloned();
        let own = self.0.me == Some(fp);
        let fork = |detail: String| {
            alarm(
                AlarmKind::Fork,
                AlarmObject::Checkpoint,
                Some(fp.to_vec()),
                detail,
            )
        };
        if let Some(p) = &pin
            && seq < p.seq
        {
            let who = if own { "this device" } else { "a device" };
            return Err(self.raise(alarm(
                AlarmKind::Rollback,
                AlarmObject::Checkpoint,
                Some(fp.to_vec()),
                format!("{who}'s head is {seq}, pinned {}", p.seq),
            )));
        }
        let acl = self.acl_head().await?;
        let Some(dp) = self.device_public(&fp, &acl).await? else {
            return Ok(None);
        };
        if let Some(p) = &pin {
            // The pinned checkpoint must still be the same one (rule 2).
            match self.fetch(&fp, p.seq).await? {
                Some(at) => {
                    let (_, h) = self.open(&fp, p.seq, &at, &dp)?;
                    if h != p.hash {
                        return Err(self.raise(fork(format!("another checkpoint {}", p.seq))));
                    }
                }
                None if seq - p.seq < self.keep() => {
                    return Err(self.raise(fork(format!("pinned checkpoint {} is gone", p.seq))));
                }
                None => {}
            }
            if seq == p.seq {
                return Ok(None);
            }
        }
        let Some(sealed) = self.fetch(&fp, seq).await? else {
            return Err(self.raise(fork(format!("head {seq} can't be produced"))));
        };
        let (body, hash) = self.open(&fp, seq, &sealed, &dp)?;
        if let Some(p) = &pin {
            if seq == p.seq + 1 && body.prev != p.hash {
                return Err(self.raise(fork(format!("checkpoint {seq} doesn't follow {}", p.seq))));
            }
            for e in &body.objects {
                let k = key_of(e);
                let prev = p.body.objects.iter().find(|x| key_of(x) == k);
                if let Some(why) = prev.and_then(|x| lower_than(x, e)) {
                    return Err(self.raise(entry_alarm(AlarmKind::Fork, e, why)));
                }
            }
        }
        self.check_claims(&body, &acl).await?;
        let raised = self.lock().raise_floors(&body);
        raised.map_err(|a| self.raise(a))?;
        self.keep_device(
            fp,
            DevicePin {
                seq,
                hash,
                body: body.clone(),
            },
        )?;
        heads.insert(fp, seq);
        Ok(Some(body))
    }

    /// The ACL and header a checkpoint names, against what this client sees (rule 5).
    async fn check_claims(&self, body: &CheckpointBody, acl: &AclVersion) -> Result<()> {
        self.check_acl(body.acl, acl).await?;
        let h = body.header;
        if h.version == 0 {
            return Ok(());
        }
        let mut st = self.lock();
        match st.header_pin {
            Some(p) if h.version < p.version => Ok(()),
            Some(p) if h.version == p.version => {
                if h.hash == p.hash {
                    return Ok(());
                }
                drop(st);
                Err(self.raise(alarm(
                    AlarmKind::Fork,
                    AlarmObject::Header,
                    None,
                    format!("another header at version {}", h.version),
                )))
            }
            _ => {
                st.header_stale = true;
                self.keep_header(&mut st, h)
            }
        }
    }

    async fn check_acl(&self, claim: Head, acl: &AclVersion) -> Result<()> {
        if claim.version == 0 {
            return Ok(());
        }
        if claim.version > acl.version {
            return Err(self.raise(alarm(
                AlarmKind::Rollback,
                AlarmObject::Acl,
                None,
                format!(
                    "a device verified ACL version {}; the server serves {}",
                    claim.version, acl.version
                ),
            )));
        }
        let mut hash = self.lock().acl_hashes.get(&claim.version).copied();
        if hash.is_none() {
            let r = self.0.session.acl().entries(Some(claim.version)).await?;
            if let Some(first) = r.entries.first()
                && let Ok(v) = AclVersion::parse(first)
                && v.version == claim.version
            {
                // The chain from the pin was verified, so a version it holds is genuine.
                hash = Some(v.hash());
                self.lock().acl_hashes.insert(claim.version, v.hash());
            }
        }
        if hash != Some(claim.hash) {
            return Err(self.raise(alarm(
                AlarmKind::Fork,
                AlarmObject::Acl,
                None,
                format!("another ACL at version {}", claim.version),
            )));
        }
        Ok(())
    }

    /// Each `seen` link must match what this client knows or can fetch (rule 3).
    async fn check_seen(
        &self,
        from: &[u8; 32],
        link: Seen,
        heads: &BTreeMap<[u8; 32], u64>,
    ) -> Result<()> {
        let fork = |detail: String| {
            self.raise(alarm(
                AlarmKind::Fork,
                AlarmObject::Checkpoint,
                Some(link.device.to_vec()),
                detail,
            ))
        };
        let known = {
            let st = self.lock();
            match st.dev_pins.get(&link.device) {
                Some(p) if p.seq == link.seq => Some(p.hash),
                _ => st.known.get(&(link.device, link.seq)).copied(),
            }
        };
        if let Some(h) = known {
            if h != link.hash {
                return Err(fork(format!(
                    "device {} saw another checkpoint {}",
                    short(from),
                    link.seq
                )));
            }
            return Ok(());
        }
        let head = heads.get(&link.device).copied().unwrap_or(0);
        if link.seq > head {
            return Err(fork(format!(
                "a checkpoint {} beyond the served head {head}",
                link.seq
            )));
        }
        let acl = self.acl_head().await?;
        let Some(dp) = self.device_public(&link.device, &acl).await? else {
            return Ok(());
        };
        let Some(sealed) = self.fetch(&link.device, link.seq).await? else {
            if head - link.seq < self.keep() {
                return Err(fork(format!("checkpoint {} is gone", link.seq)));
            }
            return Ok(());
        };
        let (_, h) = self.open(&link.device, link.seq, &sealed, &dp)?;
        self.lock().known.insert((link.device, link.seq), h);
        if h != link.hash {
            return Err(fork(format!("another checkpoint {}", link.seq)));
        }
        Ok(())
    }

    /// Verify every device's newest checkpoint and raise the pins
    /// (formats.md §14.4). Returns the first alarm as an error.
    pub async fn sync(&self) -> Result<()> {
        let _one = self.0.serial.lock().await;
        self.sync_now().await
    }

    async fn sync_now(&self) -> Result<()> {
        self.ring()?;
        self.acl_head().await?;
        let mut heads = self.heads(None).await?;
        if let Some(me) = self.0.me {
            let own = self.lock().dev_pins.get(&me).map(|p| p.seq);
            let served = heads.get(&me).copied().unwrap_or(0);
            if let Some(pinned) = own
                && served < pinned
            {
                return Err(self.raise(alarm(
                    AlarmKind::Rollback,
                    AlarmObject::Checkpoint,
                    Some(me.to_vec()),
                    format!("this device's head is {served}, pinned {pinned}"),
                )));
            }
        }
        let missing = {
            let st = self.lock();
            st.dev_pins
                .iter()
                .find(|(fp, _)| !heads.contains_key(*fp))
                .map(|(fp, p)| (*fp, p.seq))
        };
        if let Some((fp, seq)) = missing {
            return Err(self.raise(alarm(
                AlarmKind::Rollback,
                AlarmObject::Checkpoint,
                Some(fp.to_vec()),
                format!("a pinned device (seq {seq}) has no head"),
            )));
        }
        let mut fresh = Vec::new();
        for (fp, seq) in heads.clone() {
            if let Some(b) = self.advance(fp, seq, &mut heads).await? {
                fresh.push(b);
            }
        }
        for b in &fresh {
            for l in &b.seen {
                self.check_seen(&b.device, *l, &heads).await?;
            }
        }
        let stale = std::mem::take(&mut self.lock().header_stale);
        if stale {
            crate::fs::read_header(&self.0.session, self.0.fs, Some(self)).await?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- out of band

    /// This client's pins (formats.md §14.5), to compare with another
    /// client's out of band.
    pub fn export_pins(&self) -> Result<Vec<u8>> {
        let st = self.lock();
        let p = ExportedPins {
            fs: self.0.fs,
            acl: st.acl_pin.unwrap_or_default(),
            header: st.header_pin.unwrap_or_default(),
            devices: st
                .dev_pins
                .iter()
                .map(|(fp, p)| Seen {
                    device: *fp,
                    seq: p.seq,
                    hash: p.hash,
                })
                .collect(),
        };
        Ok(p.encode()?)
    }

    /// Compare another client's exported pins with this one's view: each
    /// device head as a `seen` link, the ACL and header as claims. Returns
    /// the alarms (also recorded); empty when both views are one history.
    pub async fn compare_pins(&self, bytes: &[u8]) -> Result<Vec<Alarm>> {
        let _one = self.0.serial.lock().await;
        let p = ExportedPins::decode(bytes).map_err(|_| Error::format("not exported pins"))?;
        if p.fs != self.0.fs {
            return Err(Error::local("param", format!("pins of fs {}", p.fs)));
        }
        let mut found = Vec::new();
        let mut guard = |r: Result<()>| -> Result<()> {
            match r {
                Ok(()) => Ok(()),
                Err(Error::Integrity(a)) => {
                    found.push(a);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        };
        self.ring()?;
        let heads = self.heads(None).await?;
        let acl = self.acl_head().await;
        guard(match acl {
            Ok(acl) => self.check_acl(p.acl, &acl).await,
            Err(e) => Err(e),
        })?;
        guard(self.compare_header(p.header).await)?;
        for d in p.devices {
            guard(self.check_seen(&[0; 32], d, &heads).await)?;
        }
        Ok(found)
    }

    async fn compare_header(&self, h: Head) -> Result<()> {
        if h.version == 0 {
            return Ok(());
        }
        let pin = self.header_pin();
        if let Some(p) = pin
            && h.version == p.version
            && h.hash != p.hash
        {
            return Err(self.raise(alarm(
                AlarmKind::Fork,
                AlarmObject::Header,
                None,
                format!("another header at version {}", p.version),
            )));
        }
        if pin.is_none_or(|p| h.version > p.version) {
            {
                let mut st = self.lock();
                self.keep_header(&mut st, h)?;
            }
            crate::fs::read_header(&self.0.session, self.0.fs, Some(self)).await?;
        }
        Ok(())
    }
}
