// Mounts and routing (spec/sw.md §2–§3): which mount and path a URL names.

/** A directory of a tree, served under `/<name>/` (sw.md §2). */
export interface Mount {
  /** `[a-z0-9-]{1,63}`, unique. */
  name: string;
  fs: number;
  /** The tree id (16 bytes). */
  tree: Uint8Array;
  /** An absolute path in the tree, normalised: `/` or `/a/b` (no trailing `/`). */
  base: string;
  /** Index file names tried in a directory, in order. */
  index: string[];
  /** Generate a listing for a directory without an index. */
  listing: boolean;
  /** Also answers `/` and paths that name no other mount. */
  root: boolean;
}

const NAME = /^[a-z0-9-]{1,63}$/;

function bad(why: string): Error {
  return new TypeError(`mounts: ${why}`);
}

const isObject = (v: unknown): v is Record<string, unknown> =>
  typeof v === 'object' && v !== null && !Array.isArray(v);

/** A path segment (a name in a directory): not empty, `.` or `..`, and no `/` or NUL. */
const goodSegment = (s: string) => s !== '' && s !== '.' && s !== '..' && !/[/\0]/.test(s);

/** Read mounts as kept (sw.md §2), with their defaults; throws on anything invalid. */
export function parseMounts(value: unknown): Mount[] {
  if (!Array.isArray(value)) throw bad('not a list');
  const out: Mount[] = [];
  const names = new Set<string>();
  for (const v of value) {
    if (!isObject(v)) throw bad('a mount is not an object');
    const { name, fs, tree, base } = v;
    if (typeof name !== 'string' || !NAME.test(name)) throw bad(`bad name: ${String(name)}`);
    if (names.has(name)) throw bad(`repeated name: ${name}`);
    names.add(name);
    if (typeof fs !== 'number' || !Number.isInteger(fs) || fs < 0 || fs > 0xffff_ffff) {
      throw bad(`${name}: bad fs`);
    }
    if (!(tree instanceof Uint8Array) || tree.length !== 16) throw bad(`${name}: bad tree`);
    if (typeof base !== 'string' || !base.startsWith('/')) throw bad(`${name}: bad base`);
    const segs = base === '/' ? [] : base.replace(/\/$/, '').slice(1).split('/');
    if (!segs.every(goodSegment)) throw bad(`${name}: bad base: ${base}`);
    const index = v.index ?? ['index.html'];
    if (!Array.isArray(index) || !index.every((i) => typeof i === 'string' && goodSegment(i))) {
      throw bad(`${name}: bad index`);
    }
    const listing = v.listing ?? false;
    const root = v.root ?? false;
    if (typeof listing !== 'boolean' || typeof root !== 'boolean') {
      throw bad(`${name}: bad listing or root`);
    }
    if (root && out.some((m) => m.root)) throw bad('more than one root mount');
    out.push({
      name,
      fs,
      tree: Uint8Array.from(tree),
      base: `/${segs.join('/')}`,
      index: [...index] as string[],
      listing,
      root,
    });
  }
  return out;
}

/** Where a URL goes (sw.md §3). */
export type Route =
  | {
      kind: 'node';
      mount: Mount;
      /** The decoded segments below the mount's base. */
      segments: string[];
      /** The path in the tree. */
      path: string;
      /** The URL's path ends with `/`. */
      trailing: boolean;
    }
  | { kind: 'redirect'; location: string }
  | { kind: 'error'; status: 400 | 404 };

/** Percent-decode a segment as UTF-8; undefined if malformed. */
function decode(s: string): string | undefined {
  try {
    return decodeURIComponent(s);
  } catch {
    return undefined;
  }
}

/** Route `url` (sw.md §3): to a mount and a path, a redirect, or 400/404. */
export function route(mounts: readonly Mount[], url: URL): Route {
  const raw = url.pathname;
  const trailing = raw.endsWith('/');
  const parts = raw.slice(1).split('/');
  if (trailing) parts.pop();
  const segs: string[] = [];
  for (const p of parts) {
    const d = decode(p);
    if (d === undefined || !goodSegment(d)) return { kind: 'error', status: 400 };
    segs.push(d);
  }
  let mount = segs.length ? mounts.find((m) => m.name === segs[0]) : undefined;
  let rest: string[];
  if (mount) {
    rest = segs.slice(1);
    // The mount's own path is its base directory (sw.md §3).
    if (!rest.length && !trailing) return { kind: 'redirect', location: `${raw}/${url.search}` };
  } else {
    mount = mounts.find((m) => m.root);
    if (!mount) return { kind: 'error', status: 404 };
    rest = segs;
  }
  const path = mount.base === '/' ? `/${rest.join('/')}` : [mount.base, ...rest].join('/');
  return { kind: 'node', mount, segments: rest, path, trailing };
}
