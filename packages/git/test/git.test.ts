// git over zen-serve with real git (spec/git.md): git-remote-zen and
// zen-git, bundled (packages/git/dist), on each member's PATH, each member
// with a vault, a cache (its mirrors) and a git config of their own.
import { execFile, execFileSync } from 'node:child_process';
import { mkdirSync, readdirSync, readFileSync, symlinkSync, writeFileSync } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { bytes } from '@zen/client';
import type { Db } from '@zen/db';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { ROOT_DIR, startWorld, until, type World } from '../../../apps/shared/harness.js';
import { signInWorker, type WorkerContext } from '../../../apps/shared/node.js';
import { testUser } from '../../client/src/testing/index.js';
import { setupGrants } from '../src/setup.js';
import { MIGRATIONS, SCHEMA } from '../src/signin.js';
import { PUSH_EVENT, pushesTopic, readState } from '../src/store.js';

const DIST = join(ROOT_DIR, 'packages/git/dist');
const USERS = ['alice', 'bob', 'carol', 'ci'];
/** carol reads only; ci reads only (a CI consumer). */
const READ_ONLY = [{ rights: ['read' as const] }];

let w: World;
let dir: string;
let url: (repo: string) => string;
/** The test's own view of the `git` database, as ci (read). */
let ci: WorkerContext;
let db: Db;
/** RSS lines written by every Node process the members run (`rss.mjs`). */
let rssLog: string;

interface Member {
  name: string;
  home: string;
  env: NodeJS.ProcessEnv;
}
const members = new Map<string, Member>();

function member(name: string): Member {
  const m = members.get(name);
  if (!m) throw new Error(`no member ${name}`);
  return m;
}

beforeAll(async () => {
  w = await startWorld({ users: USERS, grantsOf: { carol: READ_ONLY, ci: READ_ONLY } });
  dir = await mkdtemp(join(tmpdir(), 'zen-git-'));
  const port = new URL(w.url).port;
  url = (repo) => `zen+http://localhost:${port}/1/${repo}`;
  // The fs gets a device keyslot per member (git.md §1), and the topic grants (§10).
  const ufs = await w.admin.fs(1).unlock({ passphrase: w.passphrase });
  for (const [i] of USERS.entries()) await ufs.addDevice(testUser(i + 2).device.devicePublic);
  await setupGrants(w.admin, testUser(1).identity, 1, ufs.topic('git').id);
  ufs.close();
  // git-remote-zen on PATH, as npm would link it.
  const bin = join(dir, 'bin');
  mkdirSync(bin);
  for (const name of ['git-remote-zen', 'git-remote-zen+http']) {
    symlinkSync(join(DIST, 'git-remote-zen.mjs'), join(bin, name));
  }
  rssLog = join(dir, 'rss.log');
  const rss = join(dir, 'rss.mjs');
  writeFileSync(
    rss,
    [
      "import { appendFileSync } from 'node:fs';",
      "import { basename } from 'node:path';",
      "process.on('exit', () => {",
      "  const [, script = '', arg = ''] = process.argv;",
      `  appendFileSync(${JSON.stringify(rssLog)}, \`\${basename(script)} \${arg} \${Math.round(performance.now())} \${process.resourceUsage().maxRSS}\\n\`);`,
      '});',
      '',
    ].join('\n'),
  );
  const base = Object.fromEntries(
    Object.entries(process.env).filter(([k]) => !k.startsWith('ZEN_') && !k.startsWith('GIT_')),
  );
  for (const name of USERS) {
    const home = join(dir, name);
    mkdirSync(home);
    const gitconfig = join(home, 'gitconfig');
    writeFileSync(
      gitconfig,
      `[user]\n\tname = ${name}\n\temail = ${name}@zen.test\n[init]\n\tdefaultBranch = main\n[advice]\n\tdetachedHead = false\n`,
    );
    const env: NodeJS.ProcessEnv = {
      ...base,
      HOME: home,
      PATH: `${bin}:${process.env.PATH}`,
      ZEN_GIT_VAULT: join(home, 'vault'),
      XDG_CACHE_HOME: join(home, 'cache'),
      XDG_CONFIG_HOME: join(home, 'config'),
      GIT_CONFIG_GLOBAL: gitconfig,
      GIT_CONFIG_NOSYSTEM: '1',
      NODE_OPTIONS: `--import=${rss}`,
    };
    members.set(name, { name, home, env });
    execFileSync(
      process.execPath,
      [
        join(DIST, 'zen-git.mjs'),
        'login',
        '--server',
        w.origin,
        '--device',
        w.user(name).deviceFile,
      ],
      { env },
    );
  }
  // A writer creates the database (alice); the test then reads it as ci.
  const opts = {
    integrity: 'authenticated' as const,
    schema: SCHEMA,
    migrations: MIGRATIONS,
    scheduler: false,
    warn: () => {},
  };
  const first = await signInWorker({
    url: w.url,
    origin: w.origin,
    device: w.user('alice').deviceFile,
    passphrase: w.passphrase,
    integrity: true,
  });
  await first.open('git', opts);
  await first.close();
  ci = await signInWorker({
    url: w.url,
    origin: w.origin,
    device: w.user('ci').deviceFile,
    passphrase: w.passphrase,
    integrity: true,
  });
  db = await ci.open('git', opts);
}, 180_000);

afterAll(async () => {
  await ci?.close();
  await w?.stop();
  if (dir) await rm(dir, { recursive: true, force: true });
});

interface Run {
  code: number;
  stdout: string;
  stderr: string;
}

/** Run git as `who` in `cwd` (relative to their home). */
function run(who: string, args: string[], cwd = '.', env: NodeJS.ProcessEnv = {}): Promise<Run> {
  const m = member(who);
  return new Promise((ok) => {
    execFile(
      'git',
      args,
      { cwd: join(m.home, cwd), env: { ...m.env, ...env }, maxBuffer: 1 << 26 },
      (e, stdout, stderr) => {
        ok({ code: e ? ((e as { code?: number }).code ?? 1) : 0, stdout, stderr });
      },
    );
  });
}

/** git that must succeed: its stdout. */
async function g(who: string, args: string[], cwd = '.', env: NodeJS.ProcessEnv = {}) {
  const r = await run(who, args, cwd, env);
  if (r.code !== 0)
    throw new Error(`git ${args.join(' ')} (${who}) exited ${r.code}:\n${r.stderr}`);
  return r.stdout.trim();
}

/** git that must fail: its stderr. */
async function refused(who: string, args: string[], cwd = '.') {
  const r = await run(who, args, cwd);
  if (r.code === 0) throw new Error(`git ${args.join(' ')} (${who}) succeeded:\n${r.stderr}`);
  return r.stderr;
}

/** A new commit in `repo` (a work tree under `who`'s home). */
async function commit(who: string, repo: string, msg: string, file = 'f'): Promise<string> {
  const p = join(member(who).home, repo, file);
  writeFileSync(p, `${msg}\n`, { flag: 'a' });
  await g(who, ['add', file], repo);
  await g(who, ['commit', '-q', '-m', msg], repo);
  return g(who, ['rev-parse', 'HEAD'], repo);
}

/** A local repo with `n` commits on main. */
async function newRepo(who: string, name: string, n = 3, format = 'sha1'): Promise<void> {
  await g(who, ['init', '-q', `--object-format=${format}`, name]);
  for (let i = 1; i <= n; i++) await commit(who, name, `${name} c${i}`);
}

/** zen's refs of a repo, read by the test (ci). */
async function zenRefs(repo: string): Promise<Record<string, string>> {
  return Object.fromEntries((await readState(db, repo)).refs);
}

/** The mirror of a repo in `who`'s cache. */
function mirrorOf(who: string, repo: string): string {
  const hosts = join(member(who).home, 'cache', 'zen-git');
  const [host] = readdirSync(hosts);
  return join(hosts, host!, '1', `${repo}.git`);
}

describe('round trips', () => {
  it('a first push creates the repo; a clone gets it back, tags and HEAD', async () => {
    await newRepo('alice', 'r1');
    await g('alice', ['tag', 'light'], 'r1');
    await g('alice', ['tag', '-a', '-m', 'annotated', 'ann'], 'r1');
    await g('alice', ['checkout', '-q', '-b', 'dev'], 'r1');
    await commit('alice', 'r1', 'dev 1');
    await g('alice', ['checkout', '-q', 'main'], 'r1');
    await g('alice', ['remote', 'add', 'origin', url('r1')], 'r1');
    await g('alice', ['push', '-q', 'origin', 'main', 'dev', '--tags'], 'r1');

    const st = await readState(db, 'r1');
    expect(st.repo?.head).toBe('refs/heads/main');
    expect(st.repo?.format).toBe('sha1');
    expect(st.packs).toHaveLength(1);
    expect([...st.refs.keys()].sort()).toEqual([
      'refs/heads/dev',
      'refs/heads/main',
      'refs/tags/ann',
      'refs/tags/light',
    ]);

    await g('bob', ['clone', '-q', url('r1'), 'r1']);
    expect(await g('bob', ['rev-parse', 'HEAD'], 'r1')).toBe(
      await g('alice', ['rev-parse', 'main'], 'r1'),
    );
    expect(await g('bob', ['tag'], 'r1')).toBe('ann\nlight');
    expect(await g('bob', ['cat-file', '-t', 'ann'], 'r1')).toBe('tag');
    await g('bob', ['fsck', '--strict', '--no-dangling'], 'r1');
    expect(await g('bob', ['branch', '-r'], 'r1')).toContain('origin/dev');
  });

  it('fetch after another clone pushes; a deleted branch goes with fetch --prune', async () => {
    const tip = await commit('bob', 'r1', 'bob 1');
    await g('bob', ['push', '-q', 'origin', 'main'], 'r1');
    await g('alice', ['fetch', '-q', 'origin'], 'r1');
    expect(await g('alice', ['rev-parse', 'origin/main'], 'r1')).toBe(tip);
    // A push of known objects only: a new branch at that commit, no pack.
    const packs = (await readState(db, 'r1')).packs.length;
    await g('alice', ['push', '-q', 'origin', 'origin/main:refs/heads/same'], 'r1');
    expect((await readState(db, 'r1')).packs).toHaveLength(packs);
    await g('alice', ['push', '-q', 'origin', ':dev'], 'r1');
    expect((await zenRefs('r1'))['refs/heads/dev']).toBeUndefined();
    await g('bob', ['fetch', '-q', '--prune', 'origin'], 'r1');
    expect(await g('bob', ['branch', '-r'], 'r1')).not.toContain('origin/dev');
  });

  it('non-fast-forward refused; --force; --force-with-lease, a stale lease refused', async () => {
    await g('alice', ['pull', '-q', '--ff-only', 'origin', 'main'], 'r1');
    await g('bob', ['pull', '-q', '--ff-only', 'origin', 'main'], 'r1');
    await commit('alice', 'r1', 'alice ff');
    await g('alice', ['push', '-q', 'origin', 'main'], 'r1');
    await commit('bob', 'r1', 'bob diverges');
    expect(await refused('bob', ['push', '-q', 'origin', 'main'], 'r1')).toMatch(/rejected/);
    const forced = await g('bob', ['rev-parse', 'HEAD'], 'r1');
    await g('bob', ['push', '-q', '--force', 'origin', 'main'], 'r1');
    expect((await zenRefs('r1'))['refs/heads/main']).toBe(forced);

    await g('alice', ['fetch', '-q', 'origin'], 'r1');
    await g('alice', ['reset', '-q', '--hard', 'origin/main'], 'r1');
    await commit('alice', 'r1', 'alice lease');
    const stale = await g('alice', ['rev-parse', 'HEAD~2'], 'r1');
    expect(
      await refused(
        'alice',
        ['push', '-q', `--force-with-lease=main:${stale}`, 'origin', 'main'],
        'r1',
      ),
    ).toMatch(/stale info|rejected/);
    await g('alice', ['push', '-q', `--force-with-lease=main:${forced}`, 'origin', 'main'], 'r1');
    expect((await zenRefs('r1'))['refs/heads/main']).toBe(
      await g('alice', ['rev-parse', 'HEAD'], 'r1'),
    );
  });

  it('--atomic with one non-fast-forward ref pushes nothing', async () => {
    await g('bob', ['fetch', '-q', 'origin'], 'r1');
    await g('bob', ['reset', '-q', '--hard', 'origin/main'], 'r1');
    await g('bob', ['checkout', '-q', '-b', 't2'], 'r1');
    await commit('bob', 'r1', 't2');
    await g('bob', ['checkout', '-q', 'main'], 'r1');
    await g('bob', ['reset', '-q', '--hard', 'HEAD~1'], 'r1');
    await commit('bob', 'r1', 'not a fast-forward');
    const before = await zenRefs('r1');
    expect(await refused('bob', ['push', '-q', '--atomic', 'origin', 'main', 't2'], 'r1')).toMatch(
      /rejected|atomic/,
    );
    expect(await zenRefs('r1')).toEqual(before);
  });
});

describe('concurrent pushes', () => {
  it('two pushes to one branch from two machines: exactly one wins; the loser rebases', async () => {
    await newRepo('alice', 'race');
    await g('alice', ['push', '-q', url('race'), 'main'], 'race');
    await g('alice', ['clone', '-q', url('race'), 'race-a']);
    await g('bob', ['clone', '-q', url('race'), 'race-b']);
    await commit('alice', 'race-a', 'alice', 'a');
    await commit('bob', 'race-b', 'bob', 'b');
    const [ra, rb] = await Promise.all([
      run('alice', ['push', '-q', 'origin', 'main'], 'race-a'),
      run('bob', ['push', '-q', 'origin', 'main'], 'race-b'),
    ]);
    const codes = [ra.code, rb.code];
    expect(
      codes.filter((c) => c === 0),
      `${ra.stderr}\n${rb.stderr}`,
    ).toHaveLength(1);
    const [loser, cwd] = ra.code === 0 ? ['bob', 'race-b'] : ['alice', 'race-a'];
    expect((ra.code === 0 ? rb : ra).stderr).toMatch(/fetch first|rejected/);
    await g(loser, ['pull', '-q', '--rebase', 'origin', 'main'], cwd);
    await g(loser, ['push', '-q', 'origin', 'main'], cwd);
    await g('carol', ['clone', '-q', url('race'), 'race-c']);
    await g('carol', ['fsck', '--strict', '--no-dangling'], 'race-c');
    expect(await g('carol', ['log', '--format=%s'], 'race-c')).toMatch(
      /alice[\s\S]*bob|bob[\s\S]*alice/,
    );
  });

  it('two pushes to different branches at once: both land', async () => {
    await g('alice', ['checkout', '-q', '-b', 'fa'], 'race-a');
    await commit('alice', 'race-a', 'fa', 'fa');
    await g('bob', ['fetch', '-q', 'origin'], 'race-b');
    await g('bob', ['checkout', '-q', '-b', 'fb', 'origin/main'], 'race-b');
    await commit('bob', 'race-b', 'fb', 'fb');
    const [ra, rb] = await Promise.all([
      run('alice', ['push', '-q', 'origin', 'fa'], 'race-a'),
      run('bob', ['push', '-q', 'origin', 'fb'], 'race-b'),
    ]);
    expect([ra.code, rb.code], `${ra.stderr}\n${rb.stderr}`).toEqual([0, 0]);
    const refs = await zenRefs('race');
    expect(refs['refs/heads/fa']).toBe(await g('alice', ['rev-parse', 'fa'], 'race-a'));
    expect(refs['refs/heads/fb']).toBe(await g('bob', ['rev-parse', 'fb'], 'race-b'));
    const seqs = (await readState(db, 'race')).packs.map((p) => p.seq);
    expect(new Set(seqs).size).toBe(seqs.length);
  });
});

describe('what upload-pack serves from the mirror', () => {
  it('clone --depth 1 is shallow; --filter=blob:none is partial', async () => {
    await newRepo('alice', 'feat', 6);
    await g('alice', ['push', '-q', url('feat'), 'main'], 'feat');
    await g('bob', ['clone', '-q', '--depth', '1', url('feat'), 'feat-shallow']);
    expect(await g('bob', ['rev-list', '--count', 'HEAD'], 'feat-shallow')).toBe('1');
    expect(await g('bob', ['rev-parse', '--is-shallow-repository'], 'feat-shallow')).toBe('true');
    await g('bob', [
      'clone',
      '-q',
      '--filter=blob:none',
      '--no-checkout',
      url('feat'),
      'feat-partial',
    ]);
    const missing = (
      await g('bob', ['rev-list', '--objects', '--all', '--missing=print'], 'feat-partial')
    )
      .split('\n')
      .filter((l) => l.startsWith('?'));
    expect(missing.length).toBeGreaterThan(0);
  });

  it('a sha256 repo', async () => {
    await newRepo('alice', 'r256', 2, 'sha256');
    await g('alice', ['push', '-q', url('r256'), 'main'], 'r256');
    expect((await readState(db, 'r256')).repo?.format).toBe('sha256');
    await g('bob', ['clone', '-q', url('r256'), 'r256']);
    expect(await g('bob', ['rev-parse', '--show-object-format'], 'r256')).toBe('sha256');
    expect(await g('bob', ['rev-parse', 'HEAD'], 'r256')).toHaveLength(64);
    await g('bob', ['fsck', '--strict', '--no-dangling'], 'r256');
  });
});

describe('a push interrupted after its upload', () => {
  it('leaves nothing visible, and the next push succeeds', async () => {
    await newRepo('alice', 'cut');
    await g('alice', ['push', '-q', url('cut'), 'main'], 'cut');
    const before = await readState(db, 'cut');
    await commit('alice', 'cut', 'lost');
    const r = await run('alice', ['push', '-q', url('cut'), 'main'], 'cut', {
      ZEN_GIT_TEST_DIE_AFTER_UPLOAD: '1',
    });
    expect(r.code).not.toBe(0);
    // The upload session it opened is still open (for a resume, git.md §4 step 2).
    const alice = await signInWorker({
      url: w.url,
      origin: w.origin,
      device: w.user('alice').deviceFile,
      passphrase: w.passphrase,
    });
    try {
      expect((await alice.fs.tree(before.repo!.tree).uploads()).length).toBeGreaterThan(0);
    } finally {
      await alice.close();
    }
    const after = await readState(db, 'cut');
    expect(after.refs).toEqual(before.refs);
    expect(after.packs.map((p) => p.hash)).toEqual(before.packs.map((p) => p.hash));
    expect(after.repo?.next_seq).toBe(before.repo?.next_seq);
    // The mirror's ref didn't move either.
    expect(await g('alice', ['--git-dir', mirrorOf('alice', 'cut'), 'rev-parse', 'main'])).toBe(
      before.refs.get('refs/heads/main'),
    );
    await g('alice', ['push', '-q', url('cut'), 'main'], 'cut');
    expect((await zenRefs('cut'))['refs/heads/main']).toBe(
      await g('alice', ['rev-parse', 'HEAD'], 'cut'),
    );
  });
});

describe('the pushes topic', () => {
  it('a consumer sees one event per push, with §4 body', async () => {
    const seen: { type: string; body: Map<number, unknown> }[] = [];
    const h = db.consume(
      { group: 'ci', topic: pushesTopic(db, 'ev'), mode: 'sequential', waitMs: 500 },
      (msg) => {
        seen.push({ type: msg.type, body: msg.body as Map<number, unknown> });
      },
    );
    try {
      await newRepo('alice', 'ev', 1);
      await g('alice', ['push', '-q', url('ev'), 'main'], 'ev');
      const first = await g('alice', ['rev-parse', 'HEAD'], 'ev');
      await commit('alice', 'ev', 'second');
      await g('alice', ['push', '-q', url('ev'), 'main', 'main:refs/heads/other'], 'ev');
      await until(() => seen.length >= 2, 30_000, 'two push events');
      expect(seen.map((e) => e.type)).toEqual([PUSH_EVENT, PUSH_EVENT]);
      const [a, b] = seen.map((e) => e.body);
      expect(a!.get(1)).toBe('ev');
      expect(a!.get(5)).toBe(1);
      expect(b!.get(5)).toBe(2);
      const ups = b!.get(3) as Map<number, unknown>[];
      const main = ups.find((u) => u.get(1) === 'refs/heads/main')!;
      expect(bytes.hex(main.get(2) as Uint8Array)).toBe(first);
      expect(bytes.hex(main.get(3) as Uint8Array)).toBe(
        await g('alice', ['rev-parse', 'HEAD'], 'ev'),
      );
      expect(ups.find((u) => u.get(1) === 'refs/heads/other')!.get(2)).toBeNull();
      const packs = (await readState(db, 'ev')).packs;
      expect(b!.get(4)).toBe(packs.at(-1)!.hash);
      expect(bytes.equal(a!.get(2) as Uint8Array, testUser(2).device.fingerprint)).toBe(true);
    } finally {
      await h.stop();
    }
  });
});

describe('rights and sign-in', () => {
  it('a member with fs read clones, and their push is refused with nothing changed', async () => {
    await g('carol', ['clone', '-q', url('r1'), 'r1-carol']);
    const before = await readState(db, 'r1');
    await commit('carol', 'r1-carol', 'carol tries');
    const err = await refused(
      'carol',
      ['push', '-q', 'origin', 'HEAD:refs/heads/carol'],
      'r1-carol',
    );
    expect(err).toMatch(/zen: push refused: .*no write right/);
    const after = await readState(db, 'r1');
    expect(after.refs).toEqual(before.refs);
    expect(after.packs).toHaveLength(before.packs.length);
    const m = mirrorOf('carol', 'r1');
    expect(
      (await run('carol', ['--git-dir', m, 'rev-parse', '--verify', '-q', 'refs/heads/carol']))
        .code,
    ).not.toBe(0);
  });

  it('no device in the vault: a clear error, no prompt', async () => {
    const r = await run('bob', ['ls-remote', url('r1')], '.', {
      ZEN_GIT_VAULT: join(member('bob').home, 'empty-vault'),
    });
    expect(r.code).not.toBe(0);
    expect(r.stderr).toMatch(/no device in the vault .*zen-git login/);
  });

  it('a push straight into the mirror (not through the helper) is refused', async () => {
    const m = mirrorOf('alice', 'r1');
    await commit('alice', 'r1', 'into the mirror');
    expect(await refused('alice', ['push', '-q', m, 'HEAD:refs/heads/sneak'], 'r1')).toMatch(
      /push to the zen:\/\/ URL/,
    );
    expect((await zenRefs('r1'))['refs/heads/sneak']).toBeUndefined();
  });
});

describe('memory', () => {
  // ZEN_GIT_BENCH_REPO=<a repo> pushes and clones that repo's history instead
  // (STATS: this repository's, 585 commits).
  const bench = process.env.ZEN_GIT_BENCH_REPO;
  it('the helper and the hook, for a clone and a push of a few hundred commits', async () => {
    if (bench) {
      await g('alice', ['clone', '-q', '--bare', '--no-local', bench, 'big']);
    } else {
      await g('alice', ['init', '-q', '--bare', 'big']);
      await g('alice', ['clone', '-q', 'big', 'big-src']);
      // 300 commits over 60 files of ~4 KB each.
      const lines = (seed: number) =>
        Array.from({ length: 64 }, (_, i) => `${seed} ${i} ${'x'.repeat(48)}`).join('\n');
      for (let c = 0; c < 300; c++) {
        const f = `f${c % 60}`;
        writeFileSync(join(member('alice').home, 'big-src', f), lines(c));
        await g('alice', ['add', f], 'big-src');
        await g('alice', ['commit', '-q', '-m', `c${c}`], 'big-src');
      }
      await g('alice', ['push', '-q', 'origin', 'main'], 'big-src');
    }
    const branch = (await g('alice', ['symbolic-ref', '--short', 'HEAD'], 'big')) || 'main';
    writeFileSync(rssLog, '');
    const t0 = performance.now();
    await g('alice', ['push', '-q', url('big'), branch], 'big');
    const pushMs = Math.round(performance.now() - t0);
    const push = readFileSync(rssLog, 'utf8');
    writeFileSync(rssLog, '');
    const t1 = performance.now();
    await g('bob', ['clone', '-q', '--bare', url('big'), 'big.git']);
    const cloneMs = Math.round(performance.now() - t1);
    const clone = readFileSync(rssLog, 'utf8');
    /** [ms, KiB] of the processes whose line includes `what`. */
    const of = (log: string, what: string) => {
      const ls = log.split('\n').filter((l) => l.includes(what));
      expect(ls.length, `${what} in\n${log}`).toBeGreaterThan(0);
      const f = ls.map((l) => l.split(' ').slice(-2).map(Number));
      return { ms: Math.max(...f.map((x) => x[0]!)), kb: Math.max(...f.map((x) => x[1]!)) };
    };
    const r = {
      pushMs,
      cloneMs,
      pushHelper: of(push, 'git-remote-zen'),
      pushHook: of(push, 'zen-git.mjs hook '),
      cloneHelper: of(clone, 'git-remote-zen'),
      objects: (await readState(db, 'big')).packs[0]!.objects,
      packBytes: (await readState(db, 'big')).packs[0]!.size,
    };
    console.log(`git memory and time: ${JSON.stringify(r)}`);
    await g('bob', ['fsck', '--strict', '--no-dangling'], 'big.git');
    // OPS.md §6: Node's baseline with the client's wasm is ~90 MB.
    for (const p of [r.pushHelper, r.pushHook, r.cloneHelper])
      expect(p.kb).toBeLessThan(400 * 1024);
  });
});
