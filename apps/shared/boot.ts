// The browser side shared by the example apps: sign-in, unlock, opening the
// app's database on an IndexedDB local store, a status bar, and a tiny DOM
// helper. An app's ui/main.ts calls `boot(...)` and gets an `AppContext`.
//
// Tests drive the page through `window.app` (see harness.ts `openApp`).
import {
  type Client,
  connect,
  indexedDbLocalStore,
  type SealedStore,
  type Session,
  sealedStore,
  type UnlockedFs,
} from '@zen/client';
import { Db, type DbOptions } from '@zen/db';

/** What an app passes to `boot`. */
export interface BootOptions {
  /** The app's name: page title, local store name. */
  app: string;
  /** The database namespace (EXAMPLES.md "database …"). */
  ns: string;
  /** The fs the apps run in (default 1). */
  fs?: number;
  /** Options of `Db.open` (schema, migrations, …); `local` is set here. */
  db?: DbOptions;
}

/** A signed-in, unlocked app. */
export interface AppContext {
  client: Client;
  session: Session;
  fs: UnlockedFs;
  db: Db;
  local: SealedStore;
  /** The login name. */
  user: string;
  /** The element the app renders into (below the status bar). */
  root: HTMLElement;
  /** Re-render the status bar (the app calls it after its own changes). */
  status(): void;
}

/** The handle tests read: `window.app`. The app adds its own members. */
export interface AppWindow {
  app?: AppContext & { ready: boolean; [k: string]: unknown };
}

type Child = Node | string | number | false | null | undefined;
type Attrs = Record<string, string | number | boolean | EventListener | undefined>;

/**
 * `h('button', {onclick: f, class: 'x'}, 'text')`: an element. `on…`
 * attributes are listeners, `false`/`undefined` attributes are left out.
 */
export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Attrs = {},
  ...children: (Child | Child[])[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === false) continue;
    if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
    else if (k === 'value' && 'value' in el) (el as HTMLInputElement).value = String(v);
    else el.setAttribute(k, v === true ? '' : String(v));
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : String(c));
  }
  return el;
}

/** Replace an element's children. */
export function render(el: Element, ...children: (Child | Child[])[]): void {
  el.replaceChildren();
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : String(c));
  }
}

/** Show the sign-in form and resolve with what was entered. */
function signInForm(
  app: string,
  error?: string,
): Promise<{ user: string; password: string; passphrase: string }> {
  return new Promise((done) => {
    const user = h('input', {
      id: 'zen-user',
      placeholder: 'login name',
      autocomplete: 'username',
    });
    const password = h('input', {
      id: 'zen-password',
      type: 'password',
      placeholder: 'password',
      autocomplete: 'current-password',
    });
    const passphrase = h('input', {
      id: 'zen-passphrase',
      type: 'password',
      placeholder: 'fs passphrase',
    });
    const form = h(
      'form',
      {
        id: 'zen-signin',
        onsubmit: (e: Event) => {
          e.preventDefault();
          done({ user: user.value, password: password.value, passphrase: passphrase.value });
        },
      },
      h('h1', {}, app),
      error ? h('p', { class: 'zen-error', id: 'zen-error' }, error) : null,
      user,
      password,
      passphrase,
      h('button', { id: 'zen-signin-button', type: 'submit' }, 'Sign in'),
    );
    render(document.body, form);
  });
}

/**
 * Sign in (password), unlock the fs (passphrase) and open the app's
 * database on an IndexedDB local store sealed by a device key. Renders a
 * status bar and returns the context; sets `window.app`.
 */
export async function boot(opts: BootOptions): Promise<AppContext> {
  document.title = opts.app;
  const client = await connect(location.origin);
  let error: string | undefined;
  for (;;) {
    const f = await signInForm(opts.app, error);
    try {
      const session = await client.signInPassword(f.user, f.password);
      const local = sealedStore(indexedDbLocalStore(`zen-${opts.app}-${f.user}`));
      const fs = await session.fs(opts.fs ?? 1).unlock({ passphrase: f.passphrase }, { local });
      const db = await Db.open(fs, opts.ns, { ...opts.db, local });
      return mount({ client, session, fs, db, local, user: f.user });
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
}

function mount(c: Omit<AppContext, 'root' | 'status'>): AppContext {
  const bar = h('div', { id: 'zen-status' });
  const root = h('main', { id: 'app' });
  render(document.body, bar, root);
  const status = () =>
    render(
      bar,
      h('span', { id: 'zen-user-name' }, c.user),
      ' · ',
      h('span', { id: 'zen-online' }, navigator.onLine && c.db.crdt.online ? 'online' : 'offline'),
      ' · ',
      h('span', { id: 'zen-owner' }, c.db.isOwner ? 'owner tab' : 'other tab'),
      ' · ',
      h('span', { id: 'zen-pending' }, `${c.db.crdt.pending} pending`),
    );
  status();
  c.db.onOwner(status);
  c.db.crdt.onSync(status);
  addEventListener('online', status);
  addEventListener('offline', status);
  const ctx: AppContext = { ...c, root, status };
  (window as AppWindow).app = { ...ctx, ready: false };
  addEventListener('pagehide', () => void c.db.close());
  return ctx;
}

/** Mark the app ready for tests, with the members it exposes on `window.app`. */
export function ready(ctx: AppContext, extra: Record<string, unknown> = {}): void {
  (window as AppWindow).app = { ...ctx, ...extra, ready: true };
}

/** An error message for the UI. */
export function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
