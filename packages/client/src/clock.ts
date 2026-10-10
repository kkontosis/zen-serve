// The hybrid logical clock of a session (fs.md §2, formats.md §11.1,
// zendb.md §19.2): one clock stamps events, tree operations and CRDT rows.
import { randomBytes } from './bytes.js';
import type { Client } from './client.js';
import { zw } from './wasm.js';

/**
 * Where a clock keeps its last timestamp across restarts, and the
 * installation's counter actor (zendb.md §19.2) with it. A store that
 * persists the timestamp must persist the actor too: counter entries are
 * per actor, and an actor that restarts its sequence numbers would have its
 * increments ignored. Copying a store to another installation is a bug.
 */
export interface ClockStore {
  load(): bigint | undefined;
  save(last: bigint): void;
  /** The installation's actor; without these, every clock gets a fresh one. */
  loadActor?(): Uint8Array | undefined;
  saveActor?(actor: Uint8Array): void;
}

/** An in-memory clock store (the default): each process is its own installation. */
export function memoryClockStore(initial?: bigint): ClockStore {
  let v = initial;
  let actor: Uint8Array | undefined;
  return {
    load: () => v,
    save: (last) => {
      v = last;
    },
    loadActor: () => actor,
    saveActor: (a) => {
      actor = a;
    },
  };
}

/**
 * Observed timestamps further ahead of the corrected wall clock than this
 * are ignored when they come from other devices unchecked (event bodies):
 * the server only checks the timestamps it sees.
 */
const MAX_UNCHECKED_AHEAD_MS = 60_000;

/**
 * The hybrid logical clock of a device (fs.md §2, formats.md §11.1). A
 * session shares one (`Session.clock`) between its events, trees and CRDT
 * rows: timestamps only need to be unique per tree (or object) and device,
 * and a shared clock makes them unique and ordered everywhere.
 *
 * The wall clock is corrected by the server's (`/v1/info` `time_ms`, read
 * once before the first operation), so a device whose clock is off still
 * writes timestamps the server accepts.
 */
export class TreeClock {
  private readonly store: ClockStore;
  private clock: InstanceType<typeof zw.Clock>;
  private offsetMs = 0;
  private synced: Promise<void> | undefined;
  /** The highest timestamp the server accepted from this clock. */
  private accepted = 0n;
  /** The installation's counter actor (16 bytes, zendb.md §19.2). */
  readonly actor: Uint8Array;

  constructor(private readonly opts: { store?: ClockStore; now?: () => number } = {}) {
    this.store = opts.store ?? memoryClockStore();
    this.clock = new zw.Clock(this.store.load() ?? 0n);
    let actor = this.store.loadActor?.();
    if (actor?.length !== 16) {
      actor = randomBytes(16);
      this.store.saveActor?.(actor);
    }
    this.actor = actor;
  }

  /** The last timestamp issued or observed. */
  get last(): bigint {
    return this.clock.last;
  }

  /** The corrected wall clock, unix ms. */
  now(): number {
    return (this.opts.now ?? Date.now)() + this.offsetMs;
  }

  /** Read the server clock once (again with `force`): wall-clock offset and observation. */
  sync(client: Client, force = false): Promise<void> {
    if (force || !this.synced) {
      const p = this.syncNow(client);
      this.synced = p.catch((e) => {
        this.synced = undefined;
        throw e;
      });
    }
    return this.synced;
  }

  private async syncNow(client: Client): Promise<void> {
    const t0 = (this.opts.now ?? Date.now)();
    const info = await client.info();
    const t1 = (this.opts.now ?? Date.now)();
    const server = Number(info.time_ms);
    this.offsetMs = server - Math.round((t0 + t1) / 2);
    this.observe(zw.hlc(server, 0));
  }

  /** A new timestamp, later than everything seen. */
  tick(): bigint {
    const h = this.clock.tick(this.now());
    this.store.save(this.clock.last);
    return h;
  }

  /** Observe a timestamp from the server (node states, other devices' ops). */
  observe(h: bigint | undefined): void {
    if (h === undefined || h <= this.clock.last) return;
    this.clock.observe(h);
    this.store.save(this.clock.last);
  }

  /**
   * Observe a timestamp the server never checked (another device's event):
   * ignored when it is far ahead of the corrected wall clock, so a device
   * with a wrong clock can't push this one past what the server accepts.
   */
  observeUnchecked(h: bigint): void {
    if (zw.hlcMs(h) > this.now() + MAX_UNCHECKED_AHEAD_MS) return;
    this.observe(h);
  }

  /** Note a timestamp the server accepted. */
  accept(h: bigint): void {
    if (h > this.accepted) this.accepted = h;
  }

  /**
   * After `clock_skew`: the clock ran ahead of the server (a wrong wall
   * clock, persisted). Restart it at the server's time, but never before a
   * timestamp the server already took from it, so this device's own later
   * operations still win over its earlier ones.
   */
  reset(serverMs: number): void {
    const start = zw.hlc(serverMs, 0);
    this.clock.free();
    this.clock = new zw.Clock(start > this.accepted ? start : this.accepted);
    this.store.save(this.clock.last);
  }

  /** Free the WASM clock. */
  free(): void {
    this.clock.free();
  }
}

/** The session clock under its general name. */
export { TreeClock as SessionClock };
