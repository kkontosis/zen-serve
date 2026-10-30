// Integrity: device checkpoints, pins, and the alarms that report a server
// rolling back, withholding or forking what it serves (spec/formats.md §14,
// api.md §15, docs/CLIENT.md "Integrity").
import type {
  ChainHead,
  CheckpointBody,
  CheckpointChain,
  CheckpointEntry,
  DeviceSecret,
} from '@zen/wasm';
import type { AclVersion } from './acl.js';
import { compare, concat, equal, fromHex, hex, utf8 } from './bytes.js';
import { isCode, ZenError } from './errors.js';
import type { Fs, UnlockedFs } from './fs.js';
import type { SealedStore } from './local/sealed.js';
import type { Session } from './session.js';
import type { CheckpointPush, Stream, Subscription } from './stream.js';
import { sleep } from './transport.js';
import { zw } from './wasm.js';

/** What an alarm is about. */
export type AlarmObject = 'acl' | 'header' | 'checkpoint' | 'topic' | 'tree' | 'db' | 'vol' | 'svc';

/**
 * A detected attack (formats.md §14.4):
 * * `rollback`: an answer older than a pin (a withheld tail, an older value)
 * * `fork`: two histories (two bodies of one device at one seq, a `seen` link
 *   the server can't back, an entry lower than its writer's previous one)
 * * `gap`: a skipped event of a chain (formats.md §5.1)
 * * `rewrite`: another body at a chain position already seen or pinned
 * * `invalid`: bytes no device sealed or signed (a checkpoint that doesn't
 *   open or verify, a header signature that doesn't verify)
 */
export interface IntegrityAlarm {
  kind: 'rollback' | 'fork' | 'gap' | 'rewrite' | 'invalid';
  object: AlarmObject;
  /** The topic id, device fingerprint, … */
  id?: Uint8Array;
  detail: string;
}

/** The error an alarm is thrown as: code `integrity`, with the alarm. */
export class IntegrityError extends ZenError {
  constructor(readonly alarm: IntegrityAlarm) {
    super(0, 'integrity', `${alarm.kind} (${alarm.object}): ${alarm.detail}`);
  }
}

/** The device that signs checkpoints: its secret and the certificate its user issued. */
export interface Signer {
  device: DeviceSecret;
  cert: Uint8Array;
}

/** Options of `fs.integrity`. */
export interface IntegrityOptions {
  /** The signer. Without one, the client verifies only and writes no checkpoints. */
  device?: Signer;
  /** Where pins are kept (store `integrity/<fs>`). Default: memory only. */
  local?: SealedStore;
  /** Checkpoint after this many changes (default 64; 1 checkpoints every change). */
  sigEvery?: number;
  /** …or this long after the last change (default 5 000 ms). */
  delayMs?: number;
  /** Called with each alarm, before it is thrown. */
  onAlarm?: (a: IntegrityAlarm) => void;
}

/** A pinned device head: its newest verified checkpoint. */
export interface DevicePin {
  seq: bigint;
  hash: Uint8Array;
  body: CheckpointBody;
}

/** The floor of a topic: the newest head, and each writer's chain position. */
export interface TopicPin {
  head?: Uint8Array;
  /** By `hex(sender) + hex(chainId)`. */
  chains: Map<string, { dseq: bigint; dhash: Uint8Array }>;
}

const ZERO32 = new Uint8Array(32);
const NO_HEAD: ChainHead = { version: 0n, hash: ZERO32 };
/** Bytes a signed, sealed checkpoint adds to its body: lp, signature, seal (formats.md §14.2). */
const OVERHEAD = 4 + 3375 + 128;
/** Own chains carried per topic entry. */
const MAX_CHAINS = 16;
const PUT_ATTEMPTS = 6;
const ACL_KEY = utf8('acl');
const HEADER_KEY = utf8('header');
const DEV_PREFIX = 'dev/';

const store = (fs: number) => `integrity/${fs}`;

function u64(v: bigint): Uint8Array {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, v);
  return b;
}

function readU64(b: Uint8Array, at: number): bigint {
  return new DataView(b.buffer, b.byteOffset, b.byteLength).getBigUint64(at);
}

/** The id an entry is kept under: its type and id bytes (svc: service and connection). */
export function entryKey(e: CheckpointEntry): string {
  switch (e.type) {
    case 'topic':
      return `1${hex(e.topic)}`;
    case 'tree':
      return `2${hex(e.tree)}`;
    case 'db':
      return `3${hex(e.ns)}`;
    case 'vol':
      return `4${hex(e.vol)}`;
    case 'svc':
      return `5${hex(e.svc)}.${hex(e.conn)}`;
  }
}

function objectOf(e: CheckpointEntry): AlarmObject {
  return e.type;
}

function idOf(e: CheckpointEntry): Uint8Array {
  switch (e.type) {
    case 'topic':
      return e.topic;
    case 'tree':
      return e.tree;
    case 'db':
      return e.ns;
    case 'vol':
      return e.vol;
    case 'svc':
      return concat(e.svc, e.conn);
  }
}

const maxBytes = (a: Uint8Array, b: Uint8Array) => (compare(a, b) >= 0 ? a : b);

/** One writer's newer entry over its older one (own bodies, or two processes of a device). */
function mergeEntry(a: CheckpointEntry | undefined, b: CheckpointEntry): CheckpointEntry {
  if (!a || a.type !== b.type) return b;
  switch (b.type) {
    case 'topic': {
      const x = a as typeof b;
      const chains = new Map<string, CheckpointChain>();
      for (const c of [...x.chains, ...b.chains]) {
        const k = hex(c.chainId);
        const cur = chains.get(k);
        if (!cur || c.dseq >= cur.dseq) {
          chains.delete(k);
          chains.set(k, c);
        }
      }
      return {
        type: 'topic',
        topic: b.topic,
        head: maxBytes(x.head, b.head),
        chains: [...chains.values()].slice(-MAX_CHAINS),
      };
    }
    case 'tree':
      return (a as typeof b).ops > b.ops ? a : b;
    case 'db':
    case 'vol':
      return (a as typeof b).seq > b.seq ? a : b;
    case 'svc': {
      const x = a as typeof b;
      return {
        ...b,
        inAcked: x.inAcked > b.inAcked ? x.inAcked : b.inAcked,
        outHead: x.outHead > b.outHead ? x.outHead : b.outHead,
      };
    }
  }
}

/** Why `next` (a writer's newer entry) is lower than `prev` (its older one), if it is. */
function lowerThan(prev: CheckpointEntry, next: CheckpointEntry): string | undefined {
  switch (next.type) {
    case 'topic': {
      const p = prev as typeof next;
      if (compare(next.head, p.head) < 0) return 'topic head went back';
      for (const c of p.chains) {
        const n = next.chains.find((x) => equal(x.chainId, c.chainId));
        if (!n) continue;
        if (n.dseq < c.dseq) return 'chain went back';
        if (n.dseq === c.dseq && !equal(n.dhash, c.dhash)) return 'chain rewritten';
      }
      return undefined;
    }
    case 'tree': {
      const p = prev as typeof next;
      if (next.ops < p.ops) return 'tree ops went back';
      if (next.ops === p.ops && !(equal(next.chain, p.chain) && equal(next.state, p.state))) {
        return 'tree state changed at the same ops';
      }
      return undefined;
    }
    case 'db':
    case 'vol': {
      const p = prev as typeof next;
      if (next.seq < p.seq) return 'root seq went back';
      if (next.seq === p.seq && !equal(next.hash, p.hash)) return 'root changed at the same seq';
      return undefined;
    }
    case 'svc': {
      const p = prev as typeof next;
      if (next.inAcked < p.inAcked || next.outHead < p.outHead) return 'cursors went back';
      return undefined;
    }
  }
}

/** Why two entries at one position (tree ops, root seq) differ, if they do. */
function divergent(a: CheckpointEntry, b: CheckpointEntry): string | undefined {
  if (a.type === 'tree' && b.type === 'tree' && a.ops === b.ops) {
    if (!equal(a.chain, b.chain) || !equal(a.state, b.state))
      return 'tree state changed at the same ops';
  }
  if ((a.type === 'db' || a.type === 'vol') && a.type === b.type && a.seq === b.seq) {
    if (!equal(a.hash, b.hash)) return 'root changed at the same seq';
  }
  return undefined;
}

/** Encode pins for out-of-band comparison: `1 ‖ u32 fs ‖ acl ‖ header ‖ u32 n ‖ n × (fp ‖ u64 seq ‖ HC)`. */
export function encodePins(
  fs: number,
  acl: ChainHead,
  header: ChainHead,
  devs: [Uint8Array, DevicePin][],
): Uint8Array {
  const n = new Uint8Array(4);
  new DataView(n.buffer).setUint32(0, devs.length);
  const f = new Uint8Array(4);
  new DataView(f.buffer).setUint32(0, fs);
  return concat(
    new Uint8Array([1]),
    f,
    u64(acl.version),
    acl.hash,
    u64(header.version),
    header.hash,
    n,
    ...devs.flatMap(([fp, p]) => [fp, u64(p.seq), p.hash]),
  );
}

export interface Pins {
  fs: number;
  acl: ChainHead;
  header: ChainHead;
  devices: { device: Uint8Array; seq: bigint; hash: Uint8Array }[];
}

export function decodePins(b: Uint8Array): Pins {
  const fail = () => new ZenError(0, 'format', 'not exported pins');
  if (b.length < 1 + 4 + 2 * 40 + 4 || b[0] !== 1) throw fail();
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const fs = v.getUint32(1);
  const acl = { version: readU64(b, 5), hash: b.slice(13, 45) };
  const header = { version: readU64(b, 45), hash: b.slice(53, 85) };
  const n = v.getUint32(85);
  if (b.length !== 89 + n * 72) throw fail();
  const devices = [];
  for (let i = 0, at = 89; i < n; i++, at += 72) {
    devices.push({
      device: b.slice(at, at + 32),
      seq: readU64(b, at + 32),
      hash: b.slice(at + 40, at + 72),
    });
  }
  return { fs, acl, header, devices };
}

/**
 * Integrity of one fs (formats.md §14): this device's checkpoints, the pins
 * of every device's verified head and of each object, and the checks of
 * server answers against them. Get it with `fs.integrity(opts)`.
 */
export class Integrity {
  /** Every alarm raised, oldest first. */
  readonly alarms: IntegrityAlarm[] = [];
  private readonly devPins = new Map<string, DevicePin>();
  private aclPin: ChainHead | undefined;
  private headerPinV: ChainHead | undefined;
  /** Floors of topics, by hex topic id. */
  private readonly topics = new Map<string, TopicPin>();
  /** Floors of other objects, by `entryKey`. */
  private readonly floors = new Map<string, CheckpointEntry>();
  /** Own entries since the last checkpoint, by `entryKey`. */
  private readonly pending = new Map<string, CheckpointEntry>();
  /** When each own entry last changed (a counter), for leaving out the oldest. */
  private readonly changedAt = new Map<string, number>();
  private tick = 0;
  private changes = 0;
  private timer: ReturnType<typeof setTimeout> | undefined;
  /** Verified `(device, seq)` hashes beyond the pins, by `hex(fp):seq`. */
  private readonly known = new Map<string, Uint8Array>();
  /** Device public keys by hex fp, for the ACL version they were found in. */
  private devKeys = new Map<string, Uint8Array>();
  private devKeysAcl = -1n;
  /** ACL hashes by version. */
  private readonly aclHashes = new Map<bigint, Uint8Array>();
  private queue: Promise<unknown> = Promise.resolve();
  private bound: UnlockedFs | undefined;
  private closed = false;
  private untrusted = false;
  private headerStale = false;
  private limits: { maxValue: number; keep: bigint } | undefined;
  /** The signer's fingerprint. */
  readonly deviceFp: Uint8Array | undefined;

  private constructor(
    readonly fs: Fs,
    private readonly opts: IntegrityOptions,
  ) {
    this.deviceFp = opts.device ? opts.device.device.fingerprint : undefined;
  }

  /** @internal Use `fs.integrity(opts)`. */
  static async attach(fs: Fs, opts: IntegrityOptions): Promise<Integrity> {
    const i = new Integrity(fs, opts);
    await i.load();
    return i;
  }

  get session(): Session {
    return this.fs.session;
  }

  /** Whether no alarm was raised: once one is, checkpoints stop. */
  get trusted(): boolean {
    return !this.untrusted;
  }

  /** @internal The unlocked fs (for its keys); `unlock` binds it. */
  bind(u: UnlockedFs): void {
    this.bound = u;
  }

  private unlocked(): UnlockedFs {
    if (!this.bound || this.bound.closed) {
      throw new ZenError(0, 'locked', 'unlock the fs before syncing checkpoints');
    }
    return this.bound;
  }

  // ---------------------------------------------------------------- alarms

  /** Record an alarm and return the error to throw. */
  raise(a: IntegrityAlarm): IntegrityError {
    this.untrusted = true;
    this.alarms.push(a);
    clearTimeout(this.timer);
    try {
      this.opts.onAlarm?.(a);
    } catch {
      // the alarm stands
    }
    return new IntegrityError(a);
  }

  // ---------------------------------------------------------------- pins

  private async load(): Promise<void> {
    const local = this.opts.local;
    if (!local) return;
    const s = store(this.fs.id);
    for await (const [k, v] of local.list(s)) {
      const key = new TextDecoder().decode(k);
      if (key === 'acl') {
        this.session.acl.pin(v);
      } else if (key === 'header' && v.length === 40) {
        this.headerPinV = { version: readU64(v, 0), hash: v.slice(8) };
      } else if (key.startsWith(DEV_PREFIX) && v.length > 40) {
        let body: CheckpointBody;
        try {
          body = zw.decodeCheckpoint(v.subarray(40));
        } catch {
          continue;
        }
        const pin = { seq: readU64(v, 0), hash: v.slice(8, 40), body };
        this.devPins.set(key.slice(DEV_PREFIX.length), pin);
        this.raiseFloors(body);
      }
    }
    const acl = this.session.acl.pinned();
    if (acl) this.aclPin = { version: acl.version, hash: zw.aclHash(acl.docBytes) };
  }

  private async keepDevice(fp: string, pin: DevicePin): Promise<void> {
    this.devPins.set(fp, pin);
    this.known.set(`${fp}:${pin.seq}`, pin.hash);
    await this.opts.local?.put(
      store(this.fs.id),
      utf8(DEV_PREFIX + fp),
      concat(u64(pin.seq), pin.hash, zw.encodeCheckpoint(pin.body)),
    );
  }

  private async keepAcl(v: AclVersion): Promise<void> {
    const hash = zw.aclHash(v.docBytes);
    this.aclHashes.set(v.version, hash);
    if (this.aclPin && this.aclPin.version >= v.version) return;
    if (this.aclPin) this.changed();
    this.aclPin = { version: v.version, hash };
    await this.opts.local?.put(store(this.fs.id), ACL_KEY, v.bytes);
  }

  /** The pinned header `(version, HH)`, if a signed header was seen. */
  get headerPin(): ChainHead | undefined {
    return this.headerPinV;
  }

  private async keepHeader(h: ChainHead): Promise<void> {
    this.headerPinV = h;
    this.changed();
    await this.opts.local?.put(store(this.fs.id), HEADER_KEY, concat(u64(h.version), h.hash));
  }

  /**
   * @internal Check a header the server served (formats.md §12.1, clients):
   * `signed` is its `(version, HH)`, absent for an unsigned one. Raises the
   * pin, or throws a rollback or fork.
   */
  async checkHeader(signed: ChainHead | undefined): Promise<void> {
    const pin = this.headerPinV;
    if (!pin) {
      if (signed) await this.keepHeader(signed);
      return;
    }
    if (!signed) {
      throw this.raise({
        kind: 'rollback',
        object: 'header',
        detail: 'an unsigned header after a signed one',
      });
    }
    if (signed.version < pin.version) {
      throw this.raise({
        kind: 'rollback',
        object: 'header',
        detail: `header version ${signed.version}, pinned ${pin.version}`,
      });
    }
    if (signed.version === pin.version) {
      if (!equal(signed.hash, pin.hash)) {
        throw this.raise({
          kind: 'fork',
          object: 'header',
          detail: `another header at version ${pin.version}`,
        });
      }
      return;
    }
    await this.keepHeader(signed);
  }

  /** A device's pinned head: its newest verified checkpoint (this device's: its newest written). */
  deviceHead(device: Uint8Array): DevicePin | undefined {
    return this.devPins.get(hex(device));
  }

  /** The floor of a topic: the newest head and chain positions verified or written. */
  topicPin(topic: Uint8Array): TopicPin | undefined {
    return this.topics.get(hex(topic));
  }

  /** The floor of another object (tree, db, vol, svc), by its entry's key. */
  floor(key: string): CheckpointEntry | undefined {
    return this.floors.get(key);
  }

  private topicFloor(topic: Uint8Array): TopicPin {
    const k = hex(topic);
    let t = this.topics.get(k);
    if (!t) {
      t = { chains: new Map() };
      this.topics.set(k, t);
    }
    return t;
  }

  /** Raise the floors with a verified body (its writer is `body.device`). */
  private raiseFloors(body: CheckpointBody): void {
    for (const e of body.objects) this.raiseFloor(body.device, e);
  }

  private raiseFloor(writer: Uint8Array, e: CheckpointEntry): void {
    if (e.type === 'topic') {
      const t = this.topicFloor(e.topic);
      if (!t.head || compare(e.head, t.head) > 0) t.head = e.head;
      for (const c of e.chains) {
        const k = hex(writer) + hex(c.chainId);
        const cur = t.chains.get(k);
        if (!cur || c.dseq > cur.dseq) t.chains.set(k, { dseq: c.dseq, dhash: c.dhash });
      }
      return;
    }
    const k = entryKey(e);
    const cur = this.floors.get(k);
    // Two writers at one position with different states: two histories.
    const why = cur && divergent(cur, e);
    if (why) throw this.raise({ kind: 'fork', object: objectOf(e), id: idOf(e), detail: why });
    this.floors.set(k, mergeEntry(cur, e));
  }

  // ---------------------------------------------------------------- writing

  /**
   * Record an object entry this device wrote or verified (6.4 and 6.5 note
   * trees, connections, namespaces and volumes; topics note themselves).
   * With `count` (default), it counts toward the cadence.
   */
  note(entry: CheckpointEntry, opts: { count?: boolean } = {}): void {
    this.raiseFloor(this.deviceFp ?? ZERO32, entry);
    if (!this.deviceFp || this.closed) return;
    const k = entryKey(entry);
    this.pending.set(k, mergeEntry(this.pending.get(k), entry));
    this.changedAt.set(k, ++this.tick);
    if (opts.count !== false) this.changed();
  }

  /**
   * Check a server answer about an object against the floors (formats.md
   * §14.4), then `note` it. The same tree `ops` (or root `seq`) with another
   * state is a fork; below the floor is a rollback. `floorAt` is the floor
   * when the answer was asked for (`floor(key)` then; `null`: none): a floor
   * raised by checkpoints that arrived meanwhile may be newer than the answer
   * honestly is, so the rollback check uses it instead of the current one.
   * `count` (default false): count toward the cadence, for what this device
   * wrote; what it only verified rides in its next checkpoint.
   */
  observe(
    entry: CheckpointEntry,
    opts: { count?: boolean; floorAt?: CheckpointEntry | null } = {},
  ): void {
    if (entry.type === 'topic') throw new Error('topics check their reads themselves');
    const cur = this.floors.get(entryKey(entry));
    const was = opts.floorAt === undefined ? cur : (opts.floorAt ?? undefined);
    const why = (cur && divergent(cur, entry)) || (was && divergent(was, entry));
    if (why)
      throw this.raise({ kind: 'fork', object: objectOf(entry), id: idOf(entry), detail: why });
    const low = was && lowerThan(was, entry);
    if (low)
      throw this.raise({ kind: 'rollback', object: objectOf(entry), id: idOf(entry), detail: low });
    this.note(entry, { count: opts.count ?? false });
  }

  /** Count a change toward the cadence (formats.md §14.3), and schedule a checkpoint. */
  private changed(): void {
    if (!this.deviceFp || this.closed) return;
    this.changes++;
    if (!this.trusted) return;
    const every = this.opts.sigEvery ?? 64;
    clearTimeout(this.timer);
    if (this.changes >= every) {
      void this.checkpoint().catch(() => {});
      return;
    }
    this.timer = setTimeout(
      () => void this.checkpoint().catch(() => {}),
      this.opts.delayMs ?? 5000,
    );
    (this.timer as { unref?: () => void }).unref?.();
  }

  /** @internal An append of this device committed: its chain moved, the topic reached `offset`. */
  noteAppend(topic: Uint8Array, chain: CheckpointChain, offset: Uint8Array): void {
    this.note({ type: 'topic', topic, head: offset, chains: [chain] });
  }

  /** @internal A whole read of a topic reached `offset`. */
  noteRead(topic: Uint8Array, offset: Uint8Array): void {
    const t = this.topics.get(hex(topic));
    if (t?.head && compare(offset, t.head) <= 0) return;
    this.note({ type: 'topic', topic, head: offset, chains: [] }, { count: false });
  }

  private serial<T>(fn: () => Promise<T>): Promise<T> {
    const run = this.queue.then(fn, fn);
    this.queue = run.catch(() => {});
    return run;
  }

  private async limitsOf(): Promise<{ maxValue: number; keep: bigint }> {
    if (!this.limits) {
      const l = (await this.session.client.info()).limits;
      this.limits = { maxValue: l.max_value_bytes, keep: BigInt(l.checkpoint_keep ?? 1024) };
    }
    return this.limits;
  }

  /**
   * Write a checkpoint now, if this device has changes to sign. Returns its
   * seq, or undefined when there was nothing to write (or no signer, or an
   * alarm stopped checkpoints).
   */
  checkpoint(): Promise<bigint | undefined> {
    return this.serial(() => this.checkpointNow());
  }

  private async checkpointNow(): Promise<bigint | undefined> {
    const signer = this.opts.device;
    const me = this.deviceFp;
    if (!signer || !me || !this.trusted || (!this.pending.size && !this.changes)) return undefined;
    clearTimeout(this.timer);
    const u = this.unlocked();
    const fp = hex(me);
    const acl = this.session.acl.pinned() ?? (await this.session.acl.head());
    await this.keepAcl(acl);
    for (let attempt = 1; ; attempt++) {
      const own = this.devPins.get(fp);
      const seq = (own?.seq ?? 0n) + 1n;
      const body = await this.buildBody(seq, own);
      const signed = signer.device.signCheckpoint(body);
      const sealed = u.sealKeys().sealCheckpoint(me, seq, signed);
      try {
        await this.put(seq, sealed);
      } catch (e) {
        if (!isCode(e, 'version_mismatch') || attempt >= PUT_ATTEMPTS) throw e;
        // Another process of this device took the seq: adopt its head (verified)
        // and merge ours into it (formats.md §14.3).
        await this.adoptOwnHead();
        continue;
      }
      const hash = zw.checkpointHash(zw.checkpointBodyBytes(signed));
      await this.keepDevice(fp, {
        seq,
        hash,
        body: zw.decodeCheckpoint(zw.checkpointBodyBytes(signed)),
      });
      this.pending.clear();
      this.changes = 0;
      return seq;
    }
  }

  /** Send a put, resending the same bytes while the outcome is unknown (idempotent, api.md §15.1). */
  private async put(seq: bigint, checkpoint: Uint8Array): Promise<void> {
    for (let i = 1; ; i++) {
      try {
        await this.session.call(
          '/v1/checkpoint/put',
          zw.encodeCheckpointPut,
          zw.decodeCheckpointVersion,
          {
            fs: this.fs.id,
            device: this.deviceFp!,
            seq,
            checkpoint,
          },
        );
        return;
      } catch (e) {
        const unknown = isCode(e, 'network') || isCode(e, 'commit_unknown');
        if (!unknown || i >= 4) throw e;
        await sleep(50 * 2 ** i);
      }
    }
  }

  private async buildBody(seq: bigint, own: DevicePin | undefined): Promise<CheckpointBody> {
    const me = this.deviceFp!;
    const objects = new Map<string, CheckpointEntry>();
    for (const e of own?.body.objects ?? []) objects.set(entryKey(e), e);
    for (const [k, e] of this.pending) objects.set(k, mergeEntry(objects.get(k), e));
    const seen = [...this.devPins]
      .filter(([k]) => k !== hex(me))
      .map(([k, p]) => ({ device: fromHex(k), seq: p.seq, hash: p.hash }));
    const body: CheckpointBody = {
      fs: this.fs.id,
      device: me,
      seq,
      prev: own?.hash ?? ZERO32,
      hlc: this.session.clock.tick(),
      acl: this.aclPin ?? NO_HEAD,
      header: this.headerPinV ?? NO_HEAD,
      objects: [...objects.values()],
      seen,
    };
    // Too big: leave out the entries unchanged for longest (formats.md §14.3).
    const budget = (await this.limitsOf()).maxValue - OVERHEAD;
    if (zw.encodeCheckpoint(body).length > budget) {
      const order = [...objects.keys()].sort(
        (a, b) => (this.changedAt.get(a) ?? 0) - (this.changedAt.get(b) ?? 0),
      );
      for (const k of order) {
        objects.delete(k);
        body.objects = [...objects.values()];
        if (zw.encodeCheckpoint(body).length <= budget) break;
      }
    }
    return body;
  }

  private async adoptOwnHead(): Promise<void> {
    const me = this.deviceFp!;
    const heads = await this.heads([me]);
    const seq = heads.get(hex(me));
    if (seq === undefined) return;
    await this.advance(me, seq, heads);
  }

  /** Write a last checkpoint and stop the timer. */
  async close(): Promise<void> {
    clearTimeout(this.timer);
    if (this.pending.size || this.changes) await this.checkpoint();
    this.closed = true;
  }

  // ---------------------------------------------------------------- verifying

  private async heads(devices?: Uint8Array[]): Promise<Map<string, bigint>> {
    const r = await this.session.call(
      '/v1/checkpoint/heads',
      zw.encodeCheckpointHeads,
      zw.decodeCheckpointHeadList,
      { fs: this.fs.id, ...(devices ? { devices } : {}) },
    );
    return new Map(r.heads.map((h) => [hex(h.device), h.seq]));
  }

  private async fetch(device: Uint8Array, seq: bigint): Promise<Uint8Array | undefined> {
    const r = await this.session.call(
      '/v1/checkpoint/get',
      zw.encodeCheckpointGet,
      zw.decodeCheckpointList,
      {
        fs: this.fs.id,
        device,
        from: seq,
        to: seq,
        limit: 1,
      },
    );
    const it = r.items[0];
    return it && it.seq === seq ? it.checkpoint : undefined;
  }

  /** The verified ACL head, as an alarm when its chain breaks. */
  private async aclHead(): Promise<AclVersion> {
    try {
      const v = await this.session.acl.head();
      await this.keepAcl(v);
      return v;
    } catch (e) {
      if (isCode(e, 'acl_chain')) {
        const rolled = /below/.test(e.message);
        throw this.raise({ kind: rolled ? 'rollback' : 'fork', object: 'acl', detail: e.message });
      }
      throw e;
    }
  }

  /** A device's public key: from the ACL's members, or the registry checked against them. */
  private async devicePublic(fp: Uint8Array, acl: AclVersion): Promise<Uint8Array | undefined> {
    if (this.devKeysAcl !== acl.version) {
      this.devKeys = new Map();
      for (const m of acl.doc.members) {
        for (const cert of m.devices) {
          try {
            const dp = zw.verifyDeviceCert(m.identity, cert).devicePublic;
            this.devKeys.set(hex(zw.fingerprint(dp)), dp);
          } catch {
            // a cert that doesn't verify names no device
          }
        }
      }
      this.devKeysAcl = acl.version;
    }
    const k = hex(fp);
    const known = this.devKeys.get(k);
    if (known) return known;
    const r = await this.session.call('/v1/devices/get', zw.encodeDevicesGet, zw.decodeDeviceList, {
      devices: [fp],
    });
    const d = r.devices.find((x) => equal(x.device, fp));
    if (!d) return undefined;
    const m = acl.doc.members.find((x) => equal(zw.fingerprint(x.identity), d.user));
    if (!m) return undefined;
    try {
      const dp = zw.verifyDeviceCert(m.identity, d.cert).devicePublic;
      if (!equal(zw.fingerprint(dp), fp)) return undefined;
      this.devKeys.set(k, dp);
      return dp;
    } catch {
      return undefined;
    }
  }

  /** Open and verify a checkpoint (formats.md §14.4 rule 1). */
  private open(
    fp: Uint8Array,
    seq: bigint,
    sealed: Uint8Array,
    dp: Uint8Array,
  ): { body: CheckpointBody; hash: Uint8Array } {
    const u = this.unlocked();
    let signed: Uint8Array;
    let body: CheckpointBody;
    try {
      signed = u.keysFor(sealed).openCheckpoint(fp, seq, sealed);
    } catch (e) {
      if (isCode(e, 'epoch_ahead')) throw e;
      throw this.raise({
        kind: 'invalid',
        object: 'checkpoint',
        id: fp,
        detail: `checkpoint ${seq} doesn't open`,
      });
    }
    try {
      body = zw.verifyCheckpoint(dp, signed);
    } catch {
      throw this.raise({
        kind: 'invalid',
        object: 'checkpoint',
        id: fp,
        detail: `checkpoint ${seq} doesn't verify`,
      });
    }
    if (body.fs !== this.fs.id || !equal(body.device, fp) || body.seq !== seq) {
      throw this.raise({
        kind: 'invalid',
        object: 'checkpoint',
        id: fp,
        detail: `checkpoint ${seq} names another place`,
      });
    }
    return { body, hash: zw.checkpointHash(zw.checkpointBodyBytes(signed)) };
  }

  /**
   * Verify and pin device `fp`'s checkpoint at `seq` (rules 1, 2 and 4).
   * Returns the new body, or undefined when nothing changed or the device
   * isn't a member's.
   */
  private async advance(
    fp: Uint8Array,
    seq: bigint,
    heads: Map<string, bigint>,
  ): Promise<CheckpointBody | undefined> {
    const k = hex(fp);
    const pin = this.devPins.get(k);
    const own = this.deviceFp && equal(fp, this.deviceFp);
    if (pin && seq < pin.seq) {
      throw this.raise({
        kind: 'rollback',
        object: 'checkpoint',
        id: fp,
        detail: `${own ? 'this device' : 'a device'}'s head is ${seq}, pinned ${pin.seq}`,
      });
    }
    const acl = await this.aclHead();
    const dp = await this.devicePublic(fp, acl);
    if (!dp) return undefined;
    const { keep } = await this.limitsOf();
    if (pin) {
      // The pinned checkpoint must still be the same one (rule 2).
      const at = await this.fetch(fp, pin.seq);
      if (at) {
        const v = this.open(fp, pin.seq, at, dp);
        if (!equal(v.hash, pin.hash)) {
          throw this.raise({
            kind: 'fork',
            object: 'checkpoint',
            id: fp,
            detail: `another checkpoint ${pin.seq}`,
          });
        }
      } else if (seq - pin.seq < keep) {
        throw this.raise({
          kind: 'fork',
          object: 'checkpoint',
          id: fp,
          detail: `pinned checkpoint ${pin.seq} is gone`,
        });
      }
      if (seq === pin.seq) return undefined;
    }
    const sealed = await this.fetch(fp, seq);
    if (!sealed) {
      throw this.raise({
        kind: 'fork',
        object: 'checkpoint',
        id: fp,
        detail: `head ${seq} can't be produced`,
      });
    }
    const { body, hash } = this.open(fp, seq, sealed, dp);
    if (pin && seq === pin.seq + 1n && !equal(body.prev, pin.hash)) {
      throw this.raise({
        kind: 'fork',
        object: 'checkpoint',
        id: fp,
        detail: `checkpoint ${seq} doesn't follow ${pin.seq}`,
      });
    }
    if (pin) {
      for (const e of body.objects) {
        const prev = pin.body.objects.find((x) => entryKey(x) === entryKey(e));
        const why = prev && lowerThan(prev, e);
        if (why) throw this.raise({ kind: 'fork', object: objectOf(e), id: idOf(e), detail: why });
      }
    }
    await this.checkClaims(body, acl);
    this.raiseFloors(body);
    await this.keepDevice(k, { seq, hash, body });
    heads.set(k, seq);
    return body;
  }

  /** The ACL and header a checkpoint names, against what this client sees (rule 5). */
  private async checkClaims(body: CheckpointBody, acl: AclVersion): Promise<void> {
    await this.checkAcl(body.acl, acl);
    const h = body.header;
    if (h.version === 0n) return;
    const pin = this.headerPinV;
    if (!pin || h.version > pin.version) {
      await this.keepHeader(h);
      this.headerStale = true;
    } else if (h.version === pin.version && !equal(h.hash, pin.hash)) {
      throw this.raise({
        kind: 'fork',
        object: 'header',
        detail: `another header at version ${h.version}`,
      });
    }
  }

  private async checkAcl(claim: ChainHead, acl: AclVersion): Promise<void> {
    if (claim.version === 0n) return;
    if (claim.version > acl.version) {
      throw this.raise({
        kind: 'rollback',
        object: 'acl',
        detail: `a device verified ACL version ${claim.version}; the server serves ${acl.version}`,
      });
    }
    let hash = this.aclHashes.get(claim.version);
    if (!hash) {
      const { entries } = await this.session.acl.entries(claim.version);
      const first = entries[0];
      if (first) {
        const doc = zw.decodeSignedAcl(first).doc;
        if (zw.decodeAclDoc(doc).version === claim.version) hash = zw.aclHash(doc);
      }
      // The chain from the pin was verified, so a version it holds is genuine.
      if (hash) this.aclHashes.set(claim.version, hash);
    }
    if (!hash || !equal(hash, claim.hash)) {
      throw this.raise({
        kind: 'fork',
        object: 'acl',
        detail: `another ACL at version ${claim.version}`,
      });
    }
  }

  /** Each `seen` link must match what this client knows or can fetch (rule 3). */
  private async checkSeen(
    from: Uint8Array,
    link: { device: Uint8Array; seq: bigint; hash: Uint8Array },
    heads: Map<string, bigint>,
  ): Promise<void> {
    const k = hex(link.device);
    const fork = (detail: string) =>
      this.raise({ kind: 'fork', object: 'checkpoint', id: link.device, detail });
    const pin = this.devPins.get(k);
    const knownHash = pin?.seq === link.seq ? pin.hash : this.known.get(`${k}:${link.seq}`);
    if (knownHash) {
      if (!equal(knownHash, link.hash))
        throw fork(`device ${hex(from).slice(0, 8)} saw another checkpoint ${link.seq}`);
      return;
    }
    const head = heads.get(k) ?? 0n;
    if (link.seq > head) throw fork(`a checkpoint ${link.seq} beyond the served head ${head}`);
    const acl = await this.aclHead();
    const dp = await this.devicePublic(link.device, acl);
    if (!dp) return;
    const sealed = await this.fetch(link.device, link.seq);
    if (!sealed) {
      if (head - link.seq < (await this.limitsOf()).keep)
        throw fork(`checkpoint ${link.seq} is gone`);
      return;
    }
    const v = this.open(link.device, link.seq, sealed, dp);
    this.known.set(`${k}:${link.seq}`, v.hash);
    if (!equal(v.hash, link.hash)) throw fork(`another checkpoint ${link.seq}`);
  }

  /**
   * Verify every device's newest checkpoint and raise the pins (formats.md
   * §14.4); throws an `IntegrityError` on the first alarm.
   */
  sync(): Promise<void> {
    return this.serial(() => this.syncNow());
  }

  private async syncNow(): Promise<void> {
    this.unlocked();
    await this.aclHead();
    const heads = await this.heads();
    const me = this.deviceFp;
    if (me) {
      const own = this.devPins.get(hex(me));
      const served = heads.get(hex(me)) ?? 0n;
      if (own && served < own.seq) {
        throw this.raise({
          kind: 'rollback',
          object: 'checkpoint',
          id: me,
          detail: `this device's head is ${served}, pinned ${own.seq}`,
        });
      }
    }
    for (const [k, p] of this.devPins) {
      if (!heads.has(k)) {
        throw this.raise({
          kind: 'rollback',
          object: 'checkpoint',
          id: fromHex(k),
          detail: `a pinned device (seq ${p.seq}) has no head`,
        });
      }
    }
    const fresh: CheckpointBody[] = [];
    for (const [k, seq] of [...heads]) {
      const b = await this.advance(fromHex(k), seq, heads);
      if (b) fresh.push(b);
    }
    for (const b of fresh) for (const l of b.seen) await this.checkSeen(b.device, l, heads);
    if (this.headerStale) {
      this.headerStale = false;
      const h = await this.fs.header();
      h?.header.free();
    }
  }

  /**
   * Sync on every new head the server pushes (`cksub`), and after each
   * (re)subscription. Alarms go to `onAlarm` (and `alarms`); other errors to
   * `onError`. Close the returned subscription to stop.
   */
  async watch(
    stream: Stream,
    opts: { onError?: (e: unknown) => void } = {},
  ): Promise<Subscription<CheckpointPush>> {
    const kick = () =>
      void this.sync().catch((e) => {
        if (!(e instanceof IntegrityError)) opts.onError?.(e);
      });
    const sub = await stream.subscribeCheckpoints(this.fs.id, { onEvent: kick });
    kick();
    return sub;
  }

  // ---------------------------------------------------------------- out of band

  /** This client's pins, to compare with another client's (`comparePins`) out of band. */
  exportPins(): Uint8Array {
    const devs: [Uint8Array, DevicePin][] = [...this.devPins].map(([k, p]) => [fromHex(k), p]);
    devs.sort((a, b) => compare(a[0], b[0]));
    return encodePins(this.fs.id, this.aclPin ?? NO_HEAD, this.headerPinV ?? NO_HEAD, devs);
  }

  /**
   * Compare another client's exported pins with this one's view: each device
   * head as a `seen` link, the ACL and header as pins. Returns the alarms
   * (also recorded); empty when both views are one history.
   */
  compareTo(bytes: Uint8Array): Promise<IntegrityAlarm[]> {
    return this.serial(async () => {
      const p = decodePins(bytes);
      if (p.fs !== this.fs.id) throw new ZenError(0, 'param', `pins of fs ${p.fs}`);
      const found: IntegrityAlarm[] = [];
      const guard = async (f: () => Promise<void>) => {
        try {
          await f();
        } catch (e) {
          if (!(e instanceof IntegrityError)) throw e;
          found.push(e.alarm);
        }
      };
      const heads = await this.heads();
      await guard(async () => this.checkAcl(p.acl, await this.aclHead()));
      await guard(async () => {
        if (p.header.version === 0n) return;
        const pin = this.headerPinV;
        if (pin && p.header.version === pin.version && !equal(p.header.hash, pin.hash)) {
          throw this.raise({
            kind: 'fork',
            object: 'header',
            detail: `another header at version ${pin.version}`,
          });
        }
        if (!pin || p.header.version > pin.version) {
          await this.keepHeader(p.header);
          const h = await this.fs.header();
          h?.header.free();
        }
      });
      for (const d of p.devices) await guard(() => this.checkSeen(new Uint8Array(32), d, heads));
      return found;
    });
  }

  /** @see compareTo */
  comparePins(bytes: Uint8Array): Promise<IntegrityAlarm[]> {
    return this.compareTo(bytes);
  }
}
