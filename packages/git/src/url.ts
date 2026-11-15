// zen:// URLs (spec/git.md §1, §2.1).
//
//   zen://<host>[:<port>]/<fs>/<repo>        the server over HTTPS
//   zen+http://<host>[:<port>]/<fs>/<repo>   over plain HTTP (localhost, tests)

/** A parsed repo URL. */
export interface RepoUrl {
  /** The server's origin: `https://host[:port]` or `http://host[:port]`. */
  server: string;
  /** `host[:port]`, lowercase: names the mirror's directory. */
  host: string;
  /** The fs id. */
  fs: number;
  /** The repo name (§2.1), without a trailing `.git`. */
  repo: string;
  /** The URL in its canonical form. */
  url: string;
}

/** A user error: printed as `zen: <message>`, never with a stack. */
export class GitError extends Error {
  override name = 'GitError';
}

const NAME = /^[A-Za-z0-9._-]{1,100}$/;

/** Whether `name` is a valid repo name (§2.1). */
export function validRepoName(name: string): boolean {
  return NAME.test(name) && !name.startsWith('.') && !name.endsWith('.git');
}

/** The fs id from its decimal form, or undefined. */
export function parseFsId(s: string): number | undefined {
  if (!/^[1-9][0-9]{0,9}$/.test(s)) return undefined;
  const n = Number(s);
  return n <= 0xffff_ffff ? n : undefined;
}

/** Parse `zen://host/fs` (no repo): for `zen-git setup`. */
export function parseFsUrl(s: string): { server: string; host: string; fs: number } {
  const { server, host, parts } = split(s);
  const fs = parts.length === 1 ? parseFsId(parts[0]!) : undefined;
  if (fs === undefined) throw new GitError(`${s}: expected zen://<host>/<fs>`);
  return { server, host, fs };
}

/** Parse a repo URL; throws `GitError` with the reason. */
export function parseRepoUrl(s: string): RepoUrl {
  const { server, host, parts } = split(s);
  if (parts.length !== 2) throw new GitError(`${s}: expected zen://<host>/<fs>/<repo>`);
  const fs = parseFsId(parts[0]!);
  if (fs === undefined) throw new GitError(`${s}: the fs is a decimal id`);
  const repo = parts[1]!.endsWith('.git') ? parts[1]!.slice(0, -4) : parts[1]!;
  if (!validRepoName(repo)) {
    throw new GitError(
      `${s}: a repo name is 1 to 100 of [A-Za-z0-9._-], not starting with "." (spec/git.md §2.1)`,
    );
  }
  const scheme = server.startsWith('https:') ? 'zen' : 'zen+http';
  return { server, host, fs, repo, url: `${scheme}://${host}/${fs}/${repo}` };
}

function split(s: string): { server: string; host: string; parts: string[] } {
  let u: URL;
  try {
    u = new URL(s);
  } catch {
    throw new GitError(`${s}: not a URL`);
  }
  const proto = u.protocol === 'zen:' ? 'https:' : u.protocol === 'zen+http:' ? 'http:' : '';
  if (!proto) throw new GitError(`${s}: the scheme is zen:// or zen+http://`);
  if (u.username || u.password || u.search || u.hash) {
    throw new GitError(`${s}: no user, password, query or fragment`);
  }
  if (!u.host) throw new GitError(`${s}: no host`);
  const host = u.host.toLowerCase();
  const parts = u.pathname.split('/').filter((p, i, a) => p !== '' || (i > 0 && i < a.length - 1));
  if (parts.some((p) => p === '' || p !== decodeURIComponent(p))) {
    throw new GitError(`${s}: an empty or escaped path segment`);
  }
  return { server: `${proto}//${host}`, host, parts };
}
