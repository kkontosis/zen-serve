// A filesystem: its header (keyslots, epoch chain) and unlocking it
// (spec/formats.md §6, §12; api.md §4.4).
import type { DeviceSecret, FsHeader, FsKeys } from '@zen/wasm';
import { equal, utf8 } from './bytes.js';
import { ZenError } from './errors.js';
import { Kv, type Transaction, type TxnOptions, transaction } from './kv.js';
import { Topic } from './log.js';
import type { Argon2, Session } from './session.js';
import { Tree } from './tree.js';
import { zw } from './wasm.js';

/** Keyslot types (formats.md §6). */
export const SlotType = {
  passphrase: 1,
  recovery: 2,
  device: 3,
  passkeyPrf: 4,
  opaqueExport: 5,
} as const;

/** How to open a keyslot. Without one, `unlock` uses what the sign-in gave. */
export type Unlock =
  | { passphrase: string }
  | { recoveryKey: Uint8Array }
  | { device: DeviceSecret }
  /** A passkey's PRF output; `credentialId` (store id) picks the slot, else every type-4 slot is tried. */
  | { prfOutput: Uint8Array; credentialId?: Uint8Array }
  /** An OPAQUE export key; `credentialId` picks the slot, else every type-5 slot is tried. */
  | { opaqueExportKey: Uint8Array; credentialId?: Uint8Array };

/** The header as stored: decoded, with its version for compare-and-set. */
export interface StoredHeader {
  header: FsHeader;
  version: Uint8Array;
}

/** A filesystem, before unlocking. */
export class Fs {
  constructor(
    readonly session: Session,
    readonly id: number,
  ) {}

  /** The stored header, or undefined if the fs has none yet. */
  async header(): Promise<StoredHeader | undefined> {
    const r = await this.session.call('/v1/fs/header/get', zw.encodeHeaderGet, zw.decodeHeader, {
      fs: this.id,
    });
    if (!r.header || !r.version) return undefined;
    const header = zw.FsHeader.decode(r.header);
    if (header.fsId !== this.id) {
      header.free();
      throw new ZenError(0, 'format', `header of fs ${header.fsId} served for fs ${this.id}`);
    }
    return { header, version: r.version };
  }

  /** Write the header (admins): `expect` is the version read, absent for a new header. */
  async putHeader(header: FsHeader, expect?: Uint8Array): Promise<Uint8Array> {
    const r = await this.session.call(
      '/v1/fs/header/put',
      zw.encodeHeaderPut,
      zw.decodeHeaderVersion,
      { fs: this.id, header: header.encode(), ...(expect ? { expect } : {}) },
    );
    return r.version;
  }

  /**
   * Create the fs's keys and header (admins), with the slots `slots` makes
   * for the new keys. Fails with 409 `version_mismatch` if a header exists.
   */
  async init(slots: (keys: FsKeys) => Uint8Array[] | Promise<Uint8Array[]>): Promise<UnlockedFs> {
    const keys = zw.FsKeys.generate(this.id);
    const header = zw.FsHeader.create(keys);
    for (const s of await slots(keys)) header.addSlot(s);
    const version = await this.putHeader(header);
    return new UnlockedFs(this, keys, { header, version });
  }

  /** Open a keyslot of the header and return the unlocked fs. */
  async unlock(how?: Unlock): Promise<UnlockedFs> {
    const stored = await this.header();
    if (!stored) throw new ZenError(404, 'not_found', `fs ${this.id} has no header`);
    const keys = openHeader(stored.header, how ?? this.fromSession());
    if (!keys) {
      stored.header.free();
      throw new ZenError(0, 'decrypt', 'no keyslot opens with this secret');
    }
    if (keys.fsId !== this.id) throw new ZenError(0, 'format', 'a keyslot of another fs');
    return new UnlockedFs(this, keys, stored);
  }

  private fromSession(): Unlock {
    const u = this.session.unlock;
    if (u.opaqueExportKey) {
      return { opaqueExportKey: u.opaqueExportKey, credentialId: this.session.deviceFp! };
    }
    if (u.prf) return { prfOutput: u.prf.output, credentialId: u.prf.credentialId };
    throw new ZenError(0, 'param', 'the sign-in gave nothing to unlock with');
  }
}

function slotsOf(h: FsHeader, type: number): Uint8Array[] {
  return h.slots().filter((s) => zw.slotInfo(s).slotType === type);
}

function tryEach(slots: Uint8Array[], open: (s: Uint8Array) => FsKeys): FsKeys | undefined {
  for (const s of slots) {
    try {
      return open(s);
    } catch {
      // the next slot
    }
  }
  return undefined;
}

/** Open the first slot of `h` that `how` opens. */
export function openHeader(h: FsHeader, how: Unlock): FsKeys | undefined {
  if ('passphrase' in how) {
    const pw = utf8(how.passphrase);
    return tryEach(slotsOf(h, SlotType.passphrase), (s) => zw.openPassphraseSlot(s, pw));
  }
  if ('recoveryKey' in how) {
    return tryEach(slotsOf(h, SlotType.recovery), (s) => zw.openRecoverySlot(s, how.recoveryKey));
  }
  if ('device' in how) {
    const fp = how.device.fingerprint;
    const mine = slotsOf(h, SlotType.device).filter((s) => equal(zw.deviceSlotRecipient(s), fp));
    return tryEach(mine, (s) => zw.openDeviceSlot(s, how.device));
  }
  if ('prfOutput' in how) {
    const id = how.credentialId;
    const slots = slotsOf(h, SlotType.passkeyPrf).filter(
      (s) => !id || equal(zw.prfSlotParams(s).credentialId, id),
    );
    return tryEach(slots, (s) => zw.openPrfSlot(s, how.prfOutput));
  }
  const id = how.credentialId;
  const slots = slotsOf(h, SlotType.opaqueExport).filter(
    (s) => !id || equal(zw.opaqueSlotCredential(s), id),
  );
  return tryEach(slots, (s) => zw.openOpaqueSlot(s, how.opaqueExportKey));
}

/** PRF salts of a header's type-4 slots for `signInPasskey`, by credential store id (hex). */
export function prfSlotSalts(h: FsHeader): { credentialId: Uint8Array; prfSalt: Uint8Array }[] {
  return slotsOf(h, SlotType.passkeyPrf).map((s) => zw.prfSlotParams(s));
}

/**
 * An unlocked filesystem: its keys at the epoch the slot wrapped, and the
 * header for older epochs. Free it (`close`) to zeroize the keys.
 */
export class UnlockedFs {
  private readonly epochs = new Map<number, FsKeys>();
  /** KV access (stored keys are PRF tokens of path elements). */
  readonly kv: Kv;

  constructor(
    readonly fs: Fs,
    /** The keys the slot opened. */
    readonly keys: FsKeys,
    private stored: StoredHeader,
  ) {
    this.epochs.set(keys.epoch, keys);
    this.kv = new Kv(this);
  }

  get id(): number {
    return this.fs.id;
  }

  get session(): Session {
    return this.fs.session;
  }

  /** The header this fs was unlocked with (or last written). */
  get header(): FsHeader {
    return this.stored.header;
  }

  /**
   * Whether new data may be sealed: the slot wrapped the header's current
   * epoch. A stale slot (an older epoch) only reads (formats.md §12).
   */
  get writable(): boolean {
    return this.keys.epoch === this.stored.header.currentEpoch;
  }

  /** The keys to seal new data with. Throws for a stale slot. */
  sealKeys(): FsKeys {
    if (!this.writable) {
      throw new ZenError(
        0,
        'stale_slot',
        `keyslot wraps epoch ${this.keys.epoch}; the fs is at ${this.stored.header.currentEpoch}`,
      );
    }
    return this.keys;
  }

  /** The keys of `epoch` (walking the chain back). Newer epochs need a new unlock. */
  keysAt(epoch: number): FsKeys {
    const k = this.epochs.get(epoch);
    if (k) return k;
    if (epoch > this.keys.epoch) {
      throw new ZenError(
        0,
        'epoch_ahead',
        `data of epoch ${epoch}; these keys reach ${this.keys.epoch}: unlock again`,
      );
    }
    const older = this.stored.header.keysAt(this.keys, epoch);
    this.epochs.set(epoch, older);
    return older;
  }

  /** The keys that open a sealed object (by its header's epoch). */
  keysFor(sealed: Uint8Array): FsKeys {
    return this.keysAt(zw.peekSealed(sealed).epoch);
  }

  /** Zeroize every key. */
  close(): void {
    for (const k of this.epochs.values()) k.free();
    this.epochs.clear();
  }

  /** Run a transaction (`kv.ts`). */
  transaction<T>(fn: (tx: Transaction) => Promise<T>, opts?: TxnOptions): Promise<T> {
    return transaction(this, fn, opts);
  }

  /** A topic by path (`log.ts`). */
  topic(...path: (string | Uint8Array)[]): Topic {
    return new Topic(this, path);
  }

  /** A filesystem tree by id (`tree.ts`). */
  tree(id: Uint8Array): Tree {
    return new Tree(this, id);
  }

  // ------------------------------------------------------------ keyslot admin

  /**
   * Change the header (admins): `edit` gets the freshly read header and
   * returns whether to write it. Retries on a concurrent change.
   */
  async editHeader(edit: (h: FsHeader) => boolean | Promise<boolean>, attempts = 3): Promise<void> {
    for (let i = 1; ; i++) {
      const fresh = await this.fs.header();
      if (!fresh) throw new ZenError(404, 'not_found', 'the header is gone');
      if (!(await edit(fresh.header))) return;
      try {
        const version = await this.fs.putHeader(fresh.header, fresh.version);
        this.stored = { header: fresh.header, version };
        return;
      } catch (e) {
        if (!(e instanceof ZenError && e.code === 'version_mismatch') || i >= attempts) throw e;
      }
    }
  }

  /** Add a keyslot (admins). Returns its id. */
  async addSlot(slot: Uint8Array): Promise<Uint8Array> {
    await this.editHeader((h) => {
      h.addSlot(slot);
      return true;
    });
    return zw.slotInfo(slot).slotId;
  }

  /** Add a passphrase slot. Argon2id defaults to the browser recommendation (256 MiB, t=3). */
  addPassphrase(passphrase: string, a: Argon2 = { m_cost_kib: 262144, t_cost: 3, p_cost: 1 }) {
    return this.addSlot(
      zw.createPassphraseSlot(this.sealKeys(), utf8(passphrase), a.m_cost_kib, a.t_cost, a.p_cost),
    );
  }

  /** Add a recovery-key slot; returns the key, to show the user once. */
  async addRecovery(): Promise<Uint8Array> {
    const r = zw.createRecoverySlot(this.sealKeys());
    await this.addSlot(r.slot);
    return r.recoveryKey;
  }

  /** Add a slot for a device (its encoded public key, verified through the ACL). */
  addDevice(devicePublic: Uint8Array) {
    return this.addSlot(zw.createDeviceSlot(this.sealKeys(), devicePublic));
  }

  /** Add a slot for a passkey's PRF output (`session.registerPasskey({prf: true})`). */
  addPasskey(credentialId: Uint8Array, prf: { salt: Uint8Array; output: Uint8Array }) {
    return this.addSlot(zw.createPrfSlot(this.sealKeys(), credentialId, prf.salt, prf.output));
  }

  /** Add a slot for an OPAQUE credential's export key. */
  addOpaque(credentialId: Uint8Array, exportKey: Uint8Array) {
    return this.addSlot(zw.createOpaqueSlot(this.sealKeys(), credentialId, exportKey));
  }

  /** Remove a keyslot by id. */
  async removeSlot(slotId: Uint8Array): Promise<boolean> {
    let removed = false;
    await this.editHeader((h) => {
      removed = h.removeSlot(slotId);
      return removed;
    });
    return removed;
  }

  /**
   * Rotate to a new epoch (revocation, formats.md §12): every old slot is
   * removed and `slots` makes the new ones for the new keys. Returns the fs
   * unlocked at the new epoch. Old data stays readable through the chain.
   */
  async rotate(slots: (keys: FsKeys) => Uint8Array[] | Promise<Uint8Array[]>): Promise<UnlockedFs> {
    let next: FsKeys | undefined;
    await this.editHeader(async (h) => {
      next?.free();
      if (this.keys.epoch !== h.currentEpoch) {
        throw new ZenError(0, 'stale_slot', 'rotate from the current epoch');
      }
      next = h.rotate(this.keys);
      for (const s of h.slots()) h.removeSlot(zw.slotInfo(s).slotId);
      for (const s of await slots(next)) h.addSlot(s);
      return true;
    });
    return new UnlockedFs(this.fs, next!, this.stored);
  }
}
