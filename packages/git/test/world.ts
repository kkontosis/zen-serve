// A world for git over zen-serve with real git (spec/git.md): git-remote-zen
// and zen-git, bundled (packages/git/dist), on each member's PATH, each member
// with a vault, a cache (its mirrors) and a git config of their own.
import { type ChildProcess, execFile, execFileSync, spawn } from 'node:child_process';
import { mkdirSync, readdirSync, symlinkSync, writeFileSync } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { Db } from '@zen/db';
import {
  ROOT_DIR,
  startWorld,
  type World,
  type WorldOptions,
} from '../../../apps/shared/harness.js';
import { signInWorker, type WorkerContext } from '../../../apps/shared/node.js';
import { testUser } from '../../client/src/testing/index.js';
import { setupGrants } from '../src/setup.js';
import { MIGRATIONS, SCHEMA } from '../src/signin.js';
import { readState } from '../src/store.js';

export const DIST = join(ROOT_DIR, 'packages/git/dist');
export const ZEN_GIT = join(DIST, 'zen-git.mjs');

export interface Member {
  name: string;
  home: string;
  env: NodeJS.ProcessEnv;
}

export interface Run {
  code: number;
  stdout: string;
  stderr: string;
}

/** The `git` database's options, for the test's own view of it. */
export const DB_OPTS = {
  integrity: 'authenticated' as const,
  schema: SCHEMA,
  migrations: MIGRATIONS,
  scheduler: false,
  warn: () => {},
};

export interface GitWorldOptions {
  users: string[];
  grantsOf?: WorldOptions['grantsOf'];
  /** The member whose view the test reads the database with (fs read is enough). */
  reader: string;
  /** The member who creates the database (fs write). */
  writer: string;
  server?: WorldOptions['server'];
}

export class GitWorld {
  readonly members = new Map<string, Member>();
  /** RSS lines written by every Node process the members run (`rss.mjs`). */
  rssLog = '';
  /** The test's own view of the `git` database. */
  db!: Db;
  private reader!: WorkerContext;
  private readonly procs = new Set<ChildProcess>();

  private constructor(
    readonly w: World,
    readonly dir: string,
  ) {}

  static async start(o: GitWorldOptions): Promise<GitWorld> {
    const w = await startWorld({
      users: o.users,
      ...(o.grantsOf ? { grantsOf: o.grantsOf } : {}),
      ...(o.server ? { server: o.server } : {}),
    });
    const t = new GitWorld(w, await mkdtemp(join(tmpdir(), 'zen-git-')));
    try {
      await t.init(o);
    } catch (e) {
      await t.stop();
      throw e;
    }
    return t;
  }

  private async init(o: GitWorldOptions): Promise<void> {
    const { w, dir } = this;
    // The fs gets a device keyslot per member (git.md §1), and the topic grants (§10).
    const ufs = await w.admin.fs(1).unlock({ passphrase: w.passphrase });
    for (const [i] of o.users.entries()) await ufs.addDevice(testUser(i + 2).device.devicePublic);
    await setupGrants(
      w.admin,
      testUser(1).identity,
      1,
      ufs.topic('git').id,
      ufs.topic('zen', 'db', 'git', 'sched').id,
    );
    ufs.close();
    // git-remote-zen on PATH, as npm would link it.
    const bin = join(dir, 'bin');
    mkdirSync(bin);
    for (const name of ['git-remote-zen', 'git-remote-zen+http']) {
      symlinkSync(join(DIST, 'git-remote-zen.mjs'), join(bin, name));
    }
    this.rssLog = join(dir, 'rss.log');
    const rss = join(dir, 'rss.mjs');
    writeFileSync(
      rss,
      [
        "import { appendFileSync } from 'node:fs';",
        "import { basename } from 'node:path';",
        "process.on('exit', () => {",
        "  const [, script = '', arg = ''] = process.argv;",
        `  appendFileSync(${JSON.stringify(this.rssLog)}, \`\${basename(script)} \${arg} \${Math.round(performance.now())} \${process.resourceUsage().maxRSS}\\n\`);`,
        '});',
        '',
      ].join('\n'),
    );
    const base = Object.fromEntries(
      Object.entries(process.env).filter(([k]) => !k.startsWith('ZEN_') && !k.startsWith('GIT_')),
    );
    for (const name of o.users) {
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
      this.members.set(name, { name, home, env });
      execFileSync(
        process.execPath,
        [ZEN_GIT, 'login', '--server', w.origin, '--device', w.user(name).deviceFile],
        { env },
      );
    }
    // A writer creates the database; the test then reads it as the reader.
    const first = await this.signIn(o.writer);
    await first.open('git', DB_OPTS);
    await first.close();
    this.reader = await this.signIn(o.reader);
    this.db = await this.reader.open('git', DB_OPTS);
  }

  /** A member signed in as a worker of their own (integrity attached). */
  signIn(name: string): Promise<WorkerContext> {
    return signInWorker({
      url: this.w.url,
      origin: this.w.origin,
      device: this.w.user(name).deviceFile,
      passphrase: this.w.passphrase,
      integrity: true,
    });
  }

  async stop(): Promise<void> {
    for (const p of this.procs) p.kill('SIGKILL');
    await this.reader?.close();
    await this.w.stop();
    await rm(this.dir, { recursive: true, force: true });
  }

  /** The `zen+http://` URL of a repo of fs 1. */
  url(repo: string): string {
    return `zen+http://localhost:${new URL(this.w.url).port}/1/${repo}`;
  }

  /** The `zen+http://` URL of fs 1. */
  get fsUrl(): string {
    return `zen+http://localhost:${new URL(this.w.url).port}/1`;
  }

  member(name: string): Member {
    const m = this.members.get(name);
    if (!m) throw new Error(`no member ${name}`);
    return m;
  }

  /** Run git as `who` in `cwd` (relative to their home). */
  run(who: string, args: string[], cwd = '.', env: NodeJS.ProcessEnv = {}): Promise<Run> {
    return this.exec('git', who, args, cwd, env);
  }

  /** Run `zen-git` as `who`. */
  zenGit(who: string, args: string[], env: NodeJS.ProcessEnv = {}): Promise<Run> {
    return this.exec(process.execPath, who, [ZEN_GIT, ...args], '.', env);
  }

  /** Start `zen-git` as `who` in the background (killed by `stop`); its stderr lines go to `lines`. */
  spawnZenGit(
    who: string,
    args: string[],
    env: NodeJS.ProcessEnv = {},
  ): { proc: ChildProcess; lines: string[]; exited: Promise<number> } {
    const m = this.member(who);
    const proc = spawn(process.execPath, [ZEN_GIT, ...args], {
      cwd: m.home,
      env: { ...m.env, ...env },
      stdio: ['ignore', 'ignore', 'pipe'],
    });
    this.procs.add(proc);
    const lines: string[] = [];
    let buf = '';
    proc.stderr!.on('data', (d: Buffer) => {
      buf += d.toString();
      const parts = buf.split('\n');
      buf = parts.pop()!;
      lines.push(...parts);
    });
    const exited = new Promise<number>((r) =>
      proc.once('exit', (code, sig) => {
        this.procs.delete(proc);
        r(code ?? (sig ? 128 : 1));
      }),
    );
    return { proc, lines, exited };
  }

  private exec(
    file: string,
    who: string,
    args: string[],
    cwd: string,
    env: NodeJS.ProcessEnv,
  ): Promise<Run> {
    const m = this.member(who);
    return new Promise((ok) => {
      execFile(
        file,
        args,
        { cwd: join(m.home, cwd), env: { ...m.env, ...env }, maxBuffer: 1 << 26 },
        (e, stdout, stderr) => {
          ok({ code: e ? ((e as { code?: number }).code ?? 1) : 0, stdout, stderr });
        },
      );
    });
  }

  /** git that must succeed: its stdout. */
  async g(who: string, args: string[], cwd = '.', env: NodeJS.ProcessEnv = {}): Promise<string> {
    const r = await this.run(who, args, cwd, env);
    if (r.code !== 0)
      throw new Error(`git ${args.join(' ')} (${who}) exited ${r.code}:\n${r.stderr}`);
    return r.stdout.trim();
  }

  /** git that must fail: its stderr. */
  async refused(who: string, args: string[], cwd = '.'): Promise<string> {
    const r = await this.run(who, args, cwd);
    if (r.code === 0) throw new Error(`git ${args.join(' ')} (${who}) succeeded:\n${r.stderr}`);
    return r.stderr;
  }

  /** A new commit in `repo` (a work tree under `who`'s home). */
  async commit(who: string, repo: string, msg: string, file = 'f'): Promise<string> {
    const p = join(this.member(who).home, repo, file);
    writeFileSync(p, `${msg}\n`, { flag: 'a' });
    await this.g(who, ['add', file], repo);
    await this.g(who, ['commit', '-q', '-m', msg], repo);
    return this.g(who, ['rev-parse', 'HEAD'], repo);
  }

  /** A local repo with `n` commits on main. */
  async newRepo(who: string, name: string, n = 3, format = 'sha1'): Promise<void> {
    await this.g(who, ['init', '-q', `--object-format=${format}`, name]);
    for (let i = 1; i <= n; i++) await this.commit(who, name, `${name} c${i}`);
  }

  /** zen's refs of a repo, read by the test. */
  async zenRefs(repo: string): Promise<Record<string, string>> {
    return Object.fromEntries((await readState(this.db, repo)).refs);
  }

  /** The mirror of a repo in `who`'s cache. */
  mirrorOf(who: string, repo: string): string {
    const hosts = join(this.member(who).home, 'cache', 'zen-git');
    const [host] = readdirSync(hosts);
    return join(hosts, host!, '1', `${repo}.git`);
  }
}
