// Enrollment (spec/sw.md §8): the content origin's page and the app, in a
// conversation by postMessage. Nothing changes on the server until the user
// confirms on the content origin. Both sides take their window as a
// parameter, so they run (and are tested) without a DOM.
import {
  bytes,
  credentialVault,
  initWasm,
  type LocalStore,
  newDevice,
  type Session,
  sealedStore,
  type UnlockedFs,
  ZenError,
  zw,
} from '@zen/client';
import type { SigningIdentity } from '@zen/wasm';
import { type Mount, parseMounts } from './mounts.js';
import { encodeMounts, MOUNTS_KEY, MOUNTS_STORE } from './start.js';

/** The messages (sw.md §8). */
export type EnrollMessage =
  | { type: 'zen-enroll-hello'; devicePublic: Uint8Array; fp: Uint8Array }
  | { type: 'zen-enroll-offer'; mounts: unknown }
  | { type: 'zen-enroll-accept' }
  | { type: 'zen-enroll-done'; cert: Uint8Array; identityPublic: Uint8Array }
  | { type: 'zen-enroll-saved' }
  | { type: 'zen-enroll-failed'; reason: string };

/** What can be posted to. */
export interface PostTarget {
  postMessage(message: unknown, targetOrigin: string): void;
}

/** A `message` event as both sides read it. */
export interface MessageLike {
  data: unknown;
  origin: string;
  source: unknown;
}

/** A window as the two sides use it. */
export interface WindowLike {
  addEventListener(type: 'message', fn: (e: MessageLike) => void): void;
  removeEventListener(type: 'message', fn: (e: MessageLike) => void): void;
}

const isMsg = (d: unknown): d is { type: string } & Record<string, unknown> =>
  typeof d === 'object' && d !== null && typeof (d as { type?: unknown }).type === 'string';

const failed = (reason: string) => new ZenError(0, 'enroll_failed', reason);

// ---------------------------------------------------------------- the page

/** What the enrollment page asks the user to confirm. */
export interface Offer {
  /** The app origin that made the offer (`event.origin`). */
  origin: string;
  mounts: Mount[];
  /** The new device's fingerprint, hex. */
  fp: string;
}

/** Options of `enrollPage`. */
export interface EnrollPageOptions {
  /** The page's window (for `message` events). */
  win: WindowLike;
  /** `window.opener`: the app. */
  opener: PostTarget | null;
  /** The trusted app origins (`/_zen/config.json`). */
  appOrigins: readonly string[];
  /** The content origin's local store (the page: `indexedDbLocalStore('zen-sw')`). */
  local: LocalStore;
  /** Ask the user; true to go on. */
  confirm(offer: Offer): Promise<boolean>;
  /** Told the fingerprint once the device key exists (to show it). */
  onDevice?(fp: string): void;
  /** Where the wasm is (default: next to the bundle, or the package in Node). */
  wasm?: URL | string;
  /** Register the worker and send it `zen-restart` (sw.md §1), after the device is kept. */
  register?(): Promise<void>;
}

/** How an enrollment ended on the page. */
export interface PageResult {
  saved: boolean;
  /** The device's fingerprint, hex. */
  fp: string;
  /** Why it wasn't saved. */
  reason?: string;
  /** The origin that enrolled it. */
  origin?: string;
  mounts?: Mount[];
}

/**
 * The content origin's side (sw.md §8 steps 2–7). Resolves once the
 * enrollment is kept, declined, or failed on the app's side; it waits as
 * long as the page is open.
 */
export async function enrollPage(o: EnrollPageOptions): Promise<PageResult> {
  if (!o.opener) return { saved: false, fp: '', reason: 'no opener' };
  await initWasm(o.wasm);
  const opener = o.opener;
  const trusted = new Set(o.appOrigins);
  const d = newDevice();
  const fp = bytes.hex(d.fp);
  o.onDevice?.(fp);
  return new Promise<PageResult>((resolve) => {
    let state: 'hello' | 'confirming' | 'accepted' | 'over' = 'hello';
    let app: string | undefined;
    let mounts: Mount[] = [];
    const end = (r: PageResult) => {
      state = 'over';
      o.win.removeEventListener('message', on);
      d.seed.fill(0);
      d.secret.free();
      resolve(r);
    };
    const post = (m: EnrollMessage) => opener.postMessage(m, app!);

    const on = (e: MessageLike) => {
      if (state === 'over' || e.source !== opener || !trusted.has(e.origin)) return;
      if (app !== undefined && e.origin !== app) return;
      const m = e.data;
      if (!isMsg(m)) return;
      if (state === 'hello' && m.type === 'zen-enroll-offer') {
        let offered: Mount[];
        try {
          offered = parseMounts(m.mounts);
        } catch {
          return;
        }
        app = e.origin;
        mounts = offered;
        state = 'confirming';
        o.confirm({ origin: app, mounts, fp }).then(
          (yes) => {
            if (state !== 'confirming') return;
            if (!yes) {
              post({ type: 'zen-enroll-failed', reason: 'declined' });
              end({ saved: false, fp, reason: 'declined', origin: app });
              return;
            }
            state = 'accepted';
            post({ type: 'zen-enroll-accept' });
          },
          (err) => {
            post({ type: 'zen-enroll-failed', reason: 'declined' });
            end({ saved: false, fp, reason: String(err), origin: app });
          },
        );
        return;
      }
      if (state === 'accepted' && m.type === 'zen-enroll-failed') {
        end({ saved: false, fp, reason: String(m.reason), origin: app });
        return;
      }
      if (state === 'accepted' && m.type === 'zen-enroll-done') {
        const { cert, identityPublic } = m;
        if (!(cert instanceof Uint8Array) || !(identityPublic instanceof Uint8Array)) return;
        // The certificate must be for this device, by the identity named.
        try {
          const c = zw.verifyDeviceCert(identityPublic, cert);
          if (!bytes.equal(c.devicePublic, d.devicePublic)) return;
        } catch {
          return;
        }
        state = 'over';
        void keep(identityPublic, cert).then(
          () => {
            post({ type: 'zen-enroll-saved' });
            end({ saved: true, fp, origin: app, mounts });
          },
          (err) => {
            post({ type: 'zen-enroll-failed', reason: 'not kept' });
            end({ saved: false, fp, reason: `not kept: ${String(err)}`, origin: app });
          },
        );
      }
    };

    async function keep(identity: Uint8Array, cert: Uint8Array): Promise<void> {
      const sealed = sealedStore(o.local);
      const vault = credentialVault(sealed);
      await vault.saveDevice({ identity, deviceSeed: d.seed, cert });
      // A stored session is the previous device's.
      await vault.forgetSession();
      await sealed.put(MOUNTS_STORE, MOUNTS_KEY, encodeMounts(mounts));
      await o.register?.();
    }

    o.win.addEventListener('message', on);
    for (const origin of trusted) {
      opener.postMessage(
        {
          type: 'zen-enroll-hello',
          devicePublic: d.devicePublic,
          fp: d.fp,
        } satisfies EnrollMessage,
        origin,
      );
    }
  });
}

// ---------------------------------------------------------------- the app

/** The popup, as the app holds it. */
export interface PopupLike extends PostTarget {
  readonly closed: boolean;
  close(): void;
}

/** Options of `enrollViewer`. */
export interface EnrollViewerOptions {
  /** A signed-in session of the member. */
  session: Session;
  /** The member's signing identity: it certifies the device key. */
  identity: SigningIdentity;
  /** An admin's identity: with it, the device is added to the ACL (sw.md §8 step 6). */
  admin?: SigningIdentity;
  /** The unlocked fs the mounts name (a device keyslot is added to each). */
  fs: readonly UnlockedFs[];
  /** The mounts to offer (sw.md §2; defaults filled in). */
  mounts: readonly Partial<Mount>[];
  /** The content origin, `scheme://host[:port]`. */
  contentOrigin: string;
  /** The app's window (for `message` events; default `globalThis`). */
  win?: WindowLike;
  /** Open the popup (default `window.open(url, 'zen-enroll', 'popup')`). */
  open?(url: string): PopupLike | null;
  /** How often to check that the popup is still open, in ms (default 500). */
  pollMs?: number;
}

/** What `enrollViewer` resolves with. */
export interface Enrolled {
  /** The device's fingerprint. */
  fp: Uint8Array;
  /** Whether it was added to the ACL (an admin's identity was given). */
  inAcl: boolean;
}

/**
 * The app's side (sw.md §8 steps 1, 4 and 6): open the enrollment page,
 * offer the mounts, and once the user confirms there, certify the device,
 * add its keyslots and (with `admin`) its ACL entry. Resolves when the page
 * has kept it; rejects (`enroll_failed`) when it is declined, fails, or
 * the popup closes first.
 */
export function enrollViewer(o: EnrollViewerOptions): Promise<Enrolled> {
  const mounts = parseMounts(o.mounts);
  const origin = new URL(o.contentOrigin).origin;
  const fsById = new Map(o.fs.map((u) => [u.id, u]));
  for (const m of mounts) {
    if (!fsById.has(m.fs)) throw new TypeError(`enrollViewer: fs ${m.fs} isn't given`);
  }
  const win = o.win ?? (globalThis as unknown as WindowLike);
  const open =
    o.open ??
    ((url: string) =>
      (globalThis as unknown as { open(u: string, n: string, f: string): PopupLike | null }).open(
        url,
        'zen-enroll',
        'popup',
      ));
  const popup = open(`${origin}/_zen/enroll.html`);
  if (!popup) return Promise.reject(failed('the popup was blocked'));

  return new Promise<Enrolled>((resolve, reject) => {
    let state: 'hello' | 'offered' | 'working' | 'done' | 'over' = 'hello';
    let devicePublic: Uint8Array | undefined;
    let fp: Uint8Array | undefined;
    let inAcl = false;
    const timer = setInterval(() => {
      if (popup.closed && state !== 'over') finish(failed('the enrollment page was closed'));
    }, o.pollMs ?? 500);
    const finish = (err?: Error) => {
      state = 'over';
      clearInterval(timer);
      win.removeEventListener('message', on);
      if (err) reject(err);
      else resolve({ fp: fp!, inAcl });
    };
    const post = (m: EnrollMessage) => popup.postMessage(m, origin);

    const on = (e: MessageLike) => {
      if (state === 'over' || e.source !== popup || e.origin !== origin) return;
      const m = e.data;
      if (!isMsg(m)) return;
      if (state === 'hello' && m.type === 'zen-enroll-hello') {
        if (!(m.devicePublic instanceof Uint8Array)) return;
        devicePublic = m.devicePublic;
        fp = zw.fingerprint(devicePublic);
        state = 'offered';
        post({ type: 'zen-enroll-offer', mounts });
        return;
      }
      if (m.type === 'zen-enroll-failed' && (state === 'offered' || state === 'done')) {
        finish(failed(String(m.reason)));
        return;
      }
      if (state === 'offered' && m.type === 'zen-enroll-accept') {
        state = 'working';
        void approve(devicePublic!).then(
          (cert) => {
            if (state !== 'working') return;
            state = 'done';
            post({ type: 'zen-enroll-done', cert, identityPublic: o.identity.publicIdentity });
          },
          (err) => {
            post({ type: 'zen-enroll-failed', reason: 'the app could not enroll the device' });
            finish(err instanceof Error ? err : failed(String(err)));
          },
        );
        return;
      }
      if (state === 'done' && m.type === 'zen-enroll-saved') finish();
    };

    async function approve(pub: Uint8Array): Promise<Uint8Array> {
      const { cert } = await o.session.approveDevice(pub, o.identity);
      for (const id of new Set(mounts.map((m) => m.fs))) await fsById.get(id)!.addDevice(pub);
      if (o.admin) {
        const me = o.identity.fingerprint;
        await o.session.acl.update(o.admin, (doc) => {
          const m = doc.members.find((x) => bytes.equal(zw.fingerprint(x.identity), me));
          if (!m) throw failed('the member is not in the ACL');
          if (!m.devices.some((c) => bytes.equal(c, cert))) m.devices.push(cert);
        });
        inAcl = true;
      }
      return cert;
    }

    win.addEventListener('message', on);
  });
}
