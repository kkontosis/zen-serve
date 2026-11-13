// The parts of @zen/sw that need no server: ranges, media types, mounts,
// routing, and the policy against spec/sw.md §4.
import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import {
  contentType,
  type Mount,
  parseMounts,
  parseRange,
  route,
  SANDBOX_CSP,
  validMediaType,
} from '../src/index.js';

describe('parseRange (sw.md §6)', () => {
  it("the server's table (statics.rs `ranges`)", () => {
    const p = (h: string) => parseRange(h, 100);
    const part = (first: number, last: number) => ({ kind: 'part', first, last });
    const unsat = { kind: 'unsatisfiable' };
    expect(p('bytes=0-9')).toEqual(part(0, 9));
    expect(p('bytes=90-')).toEqual(part(90, 99));
    expect(p('bytes=-10')).toEqual(part(90, 99));
    expect(p('bytes=-1000')).toEqual(part(0, 99));
    expect(p('bytes=50-1000')).toEqual(part(50, 99));
    expect(p('bytes=0-0')).toEqual(part(0, 0));
    expect(p('bytes=99-99')).toEqual(part(99, 99));
    expect(p('BYTES=1-2')).toEqual(part(1, 2));
    expect(p(' bytes= 3 - 4 ')).toEqual(part(3, 4));
    expect(p('bytes=100-')).toEqual(unsat);
    expect(p('bytes=100-200')).toEqual(unsat);
    expect(p('bytes=-0')).toEqual(unsat);
    expect(parseRange('bytes=0-', 0)).toEqual(unsat);
    expect(parseRange('bytes=-5', 0)).toEqual(unsat);
    for (const full of [
      'bytes=0-1,5-6',
      'bytes=x',
      'items=0-1',
      'bytes=5-1',
      'bytes=+1-2',
      'bytes=-',
      'bytes=1',
      'bytes=-+5',
      'bytes=99999999999999999999-',
      'bytes=1.5-2',
      '',
    ]) {
      expect(p(full), full).toEqual({ kind: 'full' });
    }
  });

  it('random ranges against a model', () => {
    let seed = 7;
    const rnd = (n: number) => {
      seed = (seed * 1103515245 + 12345) & 0x7fffffff;
      return seed % n;
    };
    for (let i = 0; i < 5000; i++) {
      const size = rnd(4) === 0 ? 0 : rnd(1000) + 1;
      const form = rnd(3);
      const a = rnd(1200);
      const b = rnd(1200);
      const h = form === 0 ? `bytes=${a}-${b}` : form === 1 ? `bytes=${a}-` : `bytes=-${a}`;
      // The model, straight from RFC 9110 §14.1.2.
      let want: unknown;
      if (form === 2) {
        want =
          a === 0 || size === 0
            ? { kind: 'unsatisfiable' }
            : { kind: 'part', first: Math.max(0, size - a), last: size - 1 };
      } else if (form === 0 && b < a) {
        want = { kind: 'full' };
      } else if (a >= size) {
        want = { kind: 'unsatisfiable' };
      } else {
        want = { kind: 'part', first: a, last: form === 0 ? Math.min(b, size - 1) : size - 1 };
      }
      expect(parseRange(h, size), `${h} of ${size}`).toEqual(want);
    }
  });
});

describe('media types (sw.md §5)', () => {
  it('validMediaType follows RFC 9110', () => {
    for (const ok of [
      'text/plain',
      'text/html; charset=utf-8',
      'text/html;charset=UTF-8',
      'application/vnd.api+json',
      'multipart/form-data; boundary="a b;c"',
      'text/plain; a=1; b="x\\"y"',
      "a/b; k=!#$%&'*+-.^_`|~",
    ]) {
      expect(validMediaType(ok), ok).toBe(true);
    }
    for (const bad of [
      '',
      'text',
      'text/',
      '/plain',
      'text/plain;',
      'text/plain; charset',
      'text/plain; charset=',
      'text/plain ',
      ' text/plain',
      'text/pl ain',
      'text/plain; a="unterminated',
      'text/plain\r\nX-Evil: 1',
      'text/plain; a=b\0',
      'text/plain; a="Ā"',
      'text/(plain)',
    ]) {
      expect(validMediaType(bad), JSON.stringify(bad)).toBe(false);
    }
  });

  it('contentType: the xattr if valid, else by extension, else octets', () => {
    expect(contentType('a.html')).toBe('text/html; charset=utf-8');
    expect(contentType('A.HTML')).toBe('text/html; charset=utf-8');
    expect(contentType('x.tar.ZIP')).toBe('application/zip');
    expect(contentType('p.jpeg')).toBe('image/jpeg');
    expect(contentType('p.png')).toBe('image/png');
    expect(contentType('f.woff2')).toBe('font/woff2');
    expect(contentType('s.js')).toBe('text/javascript; charset=utf-8');
    expect(contentType('README')).toBe('application/octet-stream');
    expect(contentType('x.unknown')).toBe('application/octet-stream');
    expect(contentType('x.txt', 'text/x-mine; a=1')).toBe('text/x-mine; a=1');
    expect(contentType('x.txt', 'not a type')).toBe('text/plain; charset=utf-8');
    expect(contentType('x', 'text/html\r\nSet-Cookie: a=b')).toBe('application/octet-stream');
  });
});

const tree = (n: number) => new Uint8Array(16).fill(n);

describe('mounts (sw.md §2)', () => {
  it('defaults and normalising', () => {
    const [m] = parseMounts([{ name: 'docs', fs: 1, tree: tree(1), base: '/a/b/' }]);
    expect(m).toEqual({
      name: 'docs',
      fs: 1,
      tree: tree(1),
      base: '/a/b',
      index: ['index.html'],
      listing: false,
      root: false,
    });
    expect(parseMounts([{ name: 'r', fs: 0, tree: tree(2), base: '/' }])[0]!.base).toBe('/');
  });

  it('refusals', () => {
    const ok = { name: 'a', fs: 1, tree: tree(1), base: '/' };
    const cases: [unknown, RegExp][] = [
      [{}, /not a list/],
      [[null], /not an object/],
      [[{ ...ok, name: 'A' }], /bad name/],
      [[{ ...ok, name: '' }], /bad name/],
      [[{ ...ok, name: 'x'.repeat(64) }], /bad name/],
      [[{ ...ok, name: 'a_b' }], /bad name/],
      [[ok, { ...ok, tree: tree(2) }], /repeated name/],
      [[{ ...ok, fs: -1 }], /bad fs/],
      [[{ ...ok, fs: 1.5 }], /bad fs/],
      [[{ ...ok, fs: '1' }], /bad fs/],
      [[{ ...ok, tree: new Uint8Array(15) }], /bad tree/],
      [[{ ...ok, tree: [1, 2] }], /bad tree/],
      [[{ ...ok, base: 'a' }], /bad base/],
      [[{ ...ok, base: '/a//b' }], /bad base/],
      [[{ ...ok, base: '/a/./b' }], /bad base/],
      [[{ ...ok, base: '/a/..' }], /bad base/],
      [[{ ...ok, index: ['a/b'] }], /bad index/],
      [[{ ...ok, index: ['..'] }], /bad index/],
      [[{ ...ok, index: 'index.html' }], /bad index/],
      [[{ ...ok, listing: 'yes' }], /bad listing/],
      [
        [
          { ...ok, root: true },
          { ...ok, name: 'b', root: true },
        ],
        /more than one root/,
      ],
    ];
    for (const [v, re] of cases) expect(() => parseMounts(v), JSON.stringify(v)).toThrow(re);
  });
});

describe('route (sw.md §3)', () => {
  const docs = parseMounts([
    { name: 'docs', fs: 1, tree: tree(1), base: '/site' },
    { name: 'pics', fs: 1, tree: tree(2), base: '/' },
  ]);
  const withRoot = parseMounts([
    { name: 'docs', fs: 1, tree: tree(1), base: '/site' },
    { name: 'home', fs: 1, tree: tree(3), base: '/www', root: true },
  ]);
  const at = (ms: Mount[], path: string) => route(ms, new URL(path, 'https://view.test'));
  const node = (r: ReturnType<typeof route>) =>
    r.kind === 'node' ? { mount: r.mount.name, path: r.path, trailing: r.trailing } : r;

  it('mounts, paths and encodings', () => {
    expect(node(at(docs, '/docs/a/b.html'))).toEqual({
      mount: 'docs',
      path: '/site/a/b.html',
      trailing: false,
    });
    expect(node(at(docs, '/docs/'))).toEqual({ mount: 'docs', path: '/site', trailing: true });
    expect(node(at(docs, '/docs/a/'))).toEqual({ mount: 'docs', path: '/site/a', trailing: true });
    expect(node(at(docs, '/pics/x.png'))).toEqual({
      mount: 'pics',
      path: '/x.png',
      trailing: false,
    });
    expect(node(at(docs, '/pics/'))).toEqual({ mount: 'pics', path: '/', trailing: true });
    expect(node(at(docs, '/docs/my%20file%25.txt?q=1'))).toMatchObject({
      path: '/site/my file%.txt',
    });
    expect(node(at(docs, '/docs/%CE%B1.txt'))).toMatchObject({ path: '/site/α.txt' });
    expect(node(at(docs, '/docs/a b'))).toMatchObject({ path: '/site/a b' });
  });

  it("a mount's own path redirects, keeping the query", () => {
    expect(at(docs, '/docs')).toEqual({ kind: 'redirect', location: '/docs/' });
    expect(at(docs, '/docs?x=1')).toEqual({ kind: 'redirect', location: '/docs/?x=1' });
  });

  it('refusals: 400', () => {
    for (const p of [
      '/docs/a//b',
      '/docs/%zz',
      '/docs/%C3',
      '/docs/%FF',
      '/docs/a%2Fb',
      '/docs/a%00b',
    ]) {
      expect(at(docs, p), p).toEqual({ kind: 'error', status: 400 });
    }
    // The URL parser already resolves `.` and `..`, encoded or not; what is
    // left of them is refused too.
    const raw = (path: string) => route(docs, { pathname: path, search: '' } as URL);
    for (const p of [
      '//docs/a',
      '/docs/..',
      '/docs/../x',
      '/docs/./x',
      '/docs/%2e%2e/x',
      '/docs/.',
    ]) {
      expect(raw(p), p).toEqual({ kind: 'error', status: 400 });
    }
    expect(node(at(docs, '/docs/a/../b'))).toMatchObject({ path: '/site/b' });
  });

  it('without a root mount: / and unknown names are 404', () => {
    expect(at(docs, '/')).toEqual({ kind: 'error', status: 404 });
    expect(at(docs, '/other/x')).toEqual({ kind: 'error', status: 404 });
    expect(at(docs, '/index.html')).toEqual({ kind: 'error', status: 404 });
  });

  it('a root mount answers / and what names no other mount', () => {
    expect(node(at(withRoot, '/'))).toEqual({ mount: 'home', path: '/www', trailing: true });
    expect(node(at(withRoot, '/a.css'))).toEqual({
      mount: 'home',
      path: '/www/a.css',
      trailing: false,
    });
    expect(node(at(withRoot, '/other/x'))).toEqual({
      mount: 'home',
      path: '/www/other/x',
      trailing: false,
    });
    expect(node(at(withRoot, '/docs/x'))).toMatchObject({ mount: 'docs', path: '/site/x' });
    expect(node(at(withRoot, '/home/x'))).toMatchObject({ mount: 'home', path: '/www/x' });
  });
});

describe('the policy (sw.md §4)', () => {
  it("SANDBOX_CSP is sw.md §4's text", () => {
    const md = readFileSync(new URL('../../../spec/sw.md', import.meta.url), 'utf8');
    const section = md.slice(md.indexOf('## 4. Headers'));
    const start = section.indexOf('```\n') + 4;
    const block = section.slice(start, section.indexOf('\n```', start));
    const csp = block.split('\nX-Content-Type-Options:')[0]!;
    const folded = csp
      .replace(/^Content-Security-Policy:\s*/, '')
      .replace(/\s+/g, ' ')
      .trim();
    expect(folded).toContain("script-src 'none'");
    expect(SANDBOX_CSP).toBe(folded);
  });
});
