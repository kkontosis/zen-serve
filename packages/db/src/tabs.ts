// The owner of a database among the tabs and processes of one device
// (spec/zendb.md §9.2): whoever holds the Web Lock `zen/db/<fs>/<ns>`.
// Step 6 runs consumers, the scheduler and the stream in the owner; other
// tabs run their own transactions meanwhile (serializable either way).

/** The part of `navigator.locks` (Web Locks) the election uses. */
export interface LockManagerLike {
  request(
    name: string,
    options: { mode: 'exclusive'; signal?: AbortSignal },
    callback: (lock: unknown) => Promise<unknown>,
  ): Promise<unknown>;
}

/** One instance's part in the election. */
export class Owner {
  isOwner = false;
  private readonly listeners = new Set<() => void>();
  private waiters: ((owner: boolean) => void)[] = [];
  private release: (() => void) | undefined;
  private readonly abort = new AbortController();
  private closed = false;

  /**
   * Without `locks`, the instance owns at once (unless `campaign` is
   * false). With them, it waits for the lock in the background and holds
   * it until `close()`.
   */
  constructor(locks: LockManagerLike | undefined, name: string, campaign: boolean) {
    if (!campaign) return;
    if (!locks) {
      this.set(true);
      return;
    }
    locks
      .request(name, { mode: 'exclusive', signal: this.abort.signal }, () => {
        if (this.closed) return Promise.resolve();
        this.set(true);
        return new Promise<void>((r) => {
          this.release = r;
        });
      })
      .catch(() => {}); // aborted while waiting
  }

  private set(owner: boolean): void {
    if (this.isOwner === owner) return;
    this.isOwner = owner;
    const ws = this.waiters;
    this.waiters = [];
    for (const w of ws) w(owner);
    if (owner) for (const fn of this.listeners) fn();
  }

  /** Call `fn` on becoming the owner, and at once if this instance is. */
  onOwner(fn: () => void): () => void {
    this.listeners.add(fn);
    if (this.isOwner) queueMicrotask(() => this.listeners.has(fn) && fn());
    return () => this.listeners.delete(fn);
  }

  /** Resolves with the new state at the next change. */
  changed(): Promise<boolean> {
    return new Promise((r) => this.waiters.push(r));
  }

  /** Give the role (or the place in the queue) up: the next instance takes over. */
  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.listeners.clear();
    this.set(false);
    this.abort.abort();
    this.release?.();
  }
}
