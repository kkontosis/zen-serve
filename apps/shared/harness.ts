// Test harness of the example apps, for vitest scenarios and Playwright specs.
//
// `startWorld` starts one zen-serve that also serves the built apps
// (apps/dist, as its unencrypted_dir), claims it, creates fs 1 with a
// passphrase slot, and adds members with only the grants the app asks for
// (EXAMPLES.md "Grants"). Each member can sign in with a password (the
// browser) or a device key file (Node workers).
//
// This file imports the client by relative path to its sources, so it runs
// unchanged under vitest and Playwright (which has no @zen aliases).
import { type ChildProcess, spawn } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import type { Grant } from '@zen/wasm';
import { hex } from '../../packages/client/src/bytes.js';
import {
  claim,
  connect,
  fullGrants,
  member,
  type Session,
  ZenError,
  zw,
} from '../../packages/client/src/index.js';
import {
  freePort,
  type ServerOptions,
  serviceToml,
  spawnServer,
  type TestServer,
  testUser,
} from '../../packages/client/src/testing/index.js';
import { initWasm } from '../../packages/client/src/wasm.js';

const here = dirname(fileURLToPath(import.meta.url));
/** The repository root. */
export const ROOT_DIR = resolve(here, '../..');
/** Where `npm run build:apps` puts the bundles. */
export const DIST = join(ROOT_DIR, 'apps/dist');

/** A grant every member gets: fs rights (no `topic`), or rights on a topic prefix. */
export interface GrantSpec {
  /** A topic path prefix (`['chat']`); `[]` is every topic; absent: the fs itself. */
  topic?: string[];
  rights: ('read' | 'write' | 'append' | 'consume')[];
}

/** fs `read`/`write`. */
export const FS_RW: GrantSpec = { rights: ['read', 'write'] };

/** `rights` on the topics under `path`. */
export function topicGrant(path: string[], rights: GrantSpec['rights']): GrantSpec {
  return { topic: path, rights };
}

/** A member of the test world. */
export interface WorldUser {
  name: string;
  password: string;
  /** Path of the device key file (`signInWorker({device})`). */
  deviceFile: string;
  /** The user fingerprint. */
  fp: Uint8Array;
}

/** Options of `startWorld`. */
export interface WorldOptions {
  /** Member login names (default `['alice', 'bob']`). */
  users?: string[];
  /** The grants of every member (default: fs read/write only). */
  grants?: GrantSpec[];
  /** Members whose grants differ from `grants`, by name. */
  grantsOf?: Record<string, GrantSpec[]>;
  /** Server options (limits, …); `unencrypted_dir` is set here. */
  server?: ServerOptions;
  /**
   * Relayed services of fs 1 (spec/svc.md): `tcp` services on a free port,
   * or `tcp-auth`. A service id is the topic id of its path, which only an
   * unlocked fs can compute: the server is restarted with them once the fs
   * exists.
   */
  services?: WorldService[];
}

/** A relayed service of the test world. */
export interface WorldService {
  name: string;
  /** Its topic path (conventionally `['svc', name]`). */
  path: string[];
  /** `tcp` (default) or `tcp-auth` (reached through `/v1/svc/connect`). */
  mode?: 'tcp' | 'tcp-auth';
  /** Output by key reveal, so handlers can take over (default true). */
  resume?: boolean;
  /** Other `[[service]]` settings. */
  more?: Record<string, number | boolean | string>;
}

/** A running world. */
export interface World {
  server: TestServer;
  /** The server URL (`http://127.0.0.1:port`): for Node clients. */
  url: string;
  /** The origin pages load from and sessions sign (`http://localhost:port`). */
  origin: string;
  /** The fs passphrase (fs 1). */
  passphrase: string;
  users: WorldUser[];
  /** The admin's session (seed 1, full rights). */
  admin: Session;
  /** A user by name. */
  user(name: string): WorldUser;
  /** The port of a `tcp` relayed service (`WorldOptions.services`). */
  servicePort(name: string): number;
  /** The id of a relayed service, hex (its `[[service]] id`). */
  serviceId(name: string): string;
  /** The URL of an app's page (`query` without `?`). */
  appUrl(app: string, query?: string): string;
  /** Stop the server and delete its data and the device files. */
  stop(): Promise<void>;
}

const PASSPHRASE = 'apps passphrase';

/** Start a server for the apps and set up its members. */
export async function startWorld(o: WorldOptions = {}): Promise<World> {
  await initWasm();
  const names = o.users ?? ['alice', 'bob'];
  const serverOpts: ServerOptions = {
    ...o.server,
    top: { unencrypted_dir: DIST, ...o.server?.top },
  };
  let server = await spawnServer(serverOpts);
  const ports = new Map<string, number>();
  const ids = new Map<string, string>();
  const origin = `http://localhost:${server.port}`;
  const files = await mkdtemp(join(tmpdir(), 'zen-apps-'));
  try {
    const client = await connect(server.url, { origin });
    const admin = testUser(1);
    const people = names.map((name, i) => ({ name, seed: i + 2, t: testUser(i + 2) }));
    await claim(client, server.claimToken, admin.identity, {
      members: [
        member(admin.identity, [admin.cert]),
        ...people.map((p) => member(p.t.identity, [p.t.cert])),
      ],
      grants: [...fullGrants(1, admin.fp), ...fullGrants(2, admin.fp)],
    });
    const session = await client.signInDevice(admin);
    const ufs = await session
      .fs(1)
      .init((k) => [zw.createPassphraseSlot(k, new TextEncoder().encode(PASSPHRASE), 65536, 1, 1)]);
    // Topic grants name topic ids, which only an unlocked fs can compute.
    const grants: Grant[] = [];
    for (const p of people) {
      for (const g of o.grantsOf?.[p.name] ?? o.grants ?? [FS_RW]) {
        grants.push({
          fs: 1,
          ...(g.topic
            ? { topic: g.topic.length ? ufs.topic(...g.topic).id : new Uint8Array(0) }
            : {}),
          rights: g.rights,
          subject: p.t.fp,
        });
      }
    }
    await session.acl.update(admin.identity, (doc) => {
      doc.grants.push(...grants);
    });
    if (o.services?.length) {
      const specs = [];
      for (const sv of o.services) {
        const id = ufs.topic(...sv.path).id;
        ids.set(sv.name, hex(id));
        const port = sv.mode === 'tcp-auth' ? undefined : await freePort();
        if (port !== undefined) ports.set(sv.name, port);
        specs.push({
          name: sv.name,
          fs: 1,
          id,
          ...(port !== undefined ? { port } : {}),
          ...(sv.mode ? { mode: sv.mode } : {}),
          ...(sv.resume !== undefined ? { resume: sv.resume } : {}),
          ...(sv.more ? { more: sv.more } : {}),
        });
      }
      server = await server.restart({
        ...serverOpts,
        extra: `${serverOpts.extra ?? ''}\n${serviceToml(specs)}`,
      });
    }
    ufs.close();
    const users: WorldUser[] = [];
    for (const p of people) {
      const s = await client.signInDevice(p.t);
      const password = `${p.name} password`;
      await s.setPassword(p.name, password);
      const deviceFile = join(files, `${p.name}.json`);
      await writeFile(
        deviceFile,
        JSON.stringify({
          identity_seed: hex(new Uint8Array(32).fill(p.seed)),
          device_seed: hex(new Uint8Array(32).fill(p.seed ^ 0x80)),
          cert: hex(p.t.cert),
        }),
      );
      users.push({ name: p.name, password, deviceFile, fp: p.t.fp });
    }
    return {
      server,
      url: server.url,
      origin,
      passphrase: PASSPHRASE,
      users,
      admin: session,
      user(name) {
        const u = users.find((x) => x.name === name);
        if (!u) throw new Error(`no user ${name}`);
        return u;
      },
      servicePort(name) {
        const p = ports.get(name);
        if (p === undefined) throw new Error(`no tcp service ${name}`);
        return p;
      },
      serviceId(name) {
        const id = ids.get(name);
        if (id === undefined) throw new Error(`no service ${name}`);
        return id;
      },
      appUrl: (app, query) => `${origin}/unencrypted/${app}/index.html${query ? `?${query}` : ''}`,
      async stop() {
        await server.stop();
        await rm(files, { recursive: true, force: true });
      },
    };
  } catch (e) {
    await server.stop();
    await rm(files, { recursive: true, force: true });
    throw e;
  }
}

/** The part of a Playwright `Page` the harness uses (no import of Playwright here). */
export interface PageLike {
  goto(url: string): Promise<unknown>;
  fill(selector: string, value: string): Promise<void>;
  click(selector: string): Promise<void>;
  waitForFunction(fn: () => unknown, arg?: unknown, opts?: { timeout?: number }): Promise<unknown>;
  on(event: 'console', fn: (m: { text(): string }) => void): unknown;
}

/**
 * Open an app in a page and sign in as `user`; resolves once the app set
 * `window.app.ready`. Page console lines are echoed with `log`.
 */
export async function openApp(
  page: PageLike,
  w: World,
  app: string,
  user: string,
  opts: { query?: string; log?: boolean } = {},
): Promise<void> {
  if (opts.log !== false) page.on('console', (m) => console.log(`[${app}:${user}] ${m.text()}`));
  const u = w.user(user);
  await page.goto(w.appUrl(app, opts.query));
  await page.fill('#zen-user', u.name);
  await page.fill('#zen-password', u.password);
  await page.fill('#zen-passphrase', w.passphrase);
  await page.click('#zen-signin-button');
  await page.waitForFunction(
    () => (window as unknown as { app?: { ready?: boolean } }).app?.ready === true,
    undefined,
    { timeout: 60_000 },
  );
}

/** A Node process (an app worker), killable at will. */
export interface WorkerProcess {
  child: ChildProcess;
  /** Lines it printed (stdout and stderr). */
  output: string[];
  /** Resolves with the exit code (or the signal) once it exits. */
  exited: Promise<number | string>;
  /** SIGKILL it (no clean-up runs) and wait for the exit. */
  kill(): Promise<void>;
  /** Wait until it printed a line containing `text`. */
  waitFor(text: string, ms?: number): Promise<void>;
}

/**
 * Run a built worker, `apps/dist/<app>/workers/<name>.mjs` (built by
 * `npm run build:apps` from `apps/<app>/workers/<name>.ts`), with `env`.
 */
export function spawnWorker(
  app: string,
  name: string,
  env: Record<string, string> = {},
): WorkerProcess {
  const child = spawn(process.execPath, [join(DIST, app, 'workers', `${name}.mjs`)], {
    env: { ...process.env, ...env },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const output: string[] = [];
  const waiters = new Set<() => void>();
  const onData = (b: Buffer) => {
    for (const line of b.toString().split('\n')) if (line) output.push(line);
    for (const w of waiters) w();
  };
  child.stdout!.on('data', onData);
  child.stderr!.on('data', onData);
  const exited = new Promise<number | string>((r) =>
    child.once('exit', (code, sig) => r(code ?? sig ?? -1)),
  );
  return {
    child,
    output,
    exited,
    async kill() {
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
      await exited;
    },
    waitFor(text, ms = 30_000) {
      return new Promise((ok, fail) => {
        const check = () => {
          if (output.some((l) => l.includes(text))) {
            waiters.delete(check);
            clearTimeout(t);
            ok();
          }
        };
        const t = setTimeout(() => {
          waiters.delete(check);
          fail(
            new Error(
              `worker ${name} never printed ${JSON.stringify(text)}:\n${output.join('\n')}`,
            ),
          );
        }, ms);
        waiters.add(check);
        check();
      });
    },
  };
}

/** The environment a worker gets to sign in (read by `workerEnv` in the worker). */
export function workerEnv(w: World, user: string, extra: Record<string, string> = {}) {
  return {
    ZEN_URL: w.url,
    ZEN_ORIGIN: w.origin,
    ZEN_DEVICE_FILE: w.user(user).deviceFile,
    ZEN_PASSPHRASE: w.passphrase,
    ...extra,
  };
}

export const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

/** Poll `cond` until it holds (every 25 ms), or fail after `ms`. */
export async function until(
  cond: () => boolean | Promise<boolean>,
  ms = 15_000,
  what = 'a condition',
): Promise<void> {
  const end = Date.now() + ms;
  while (!(await cond())) {
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(25);
  }
}

/** An offline switch on a Node session: while `offline`, every call fails `network`. */
export interface Net {
  offline: boolean;
  /** Paths called so far (while online). */
  calls: string[];
  count(path: string): number;
  restore(): void;
}

/** Wrap `s.call` with an offline switch. */
export function net(s: Session): Net {
  const orig = s.call.bind(s);
  const n: Net = {
    offline: false,
    calls: [],
    count: (p) => n.calls.filter((c) => c === p).length,
    restore: () => {
      s.call = orig;
    },
  };
  s.call = (async (path, enc, dec, req) => {
    if (n.offline) throw new ZenError(0, 'network', 'offline (test)');
    n.calls.push(path);
    return orig(path, enc, dec, req);
  }) as Session['call'];
  return n;
}
