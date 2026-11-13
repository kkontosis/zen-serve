// Media types (spec/sw.md §5).

const TEXT = '; charset=utf-8';

/** By extension, lowercase (sw.md §5's table). */
const BY_EXT: Record<string, string> = {
  html: `text/html${TEXT}`,
  htm: `text/html${TEXT}`,
  css: `text/css${TEXT}`,
  js: `text/javascript${TEXT}`,
  mjs: `text/javascript${TEXT}`,
  json: 'application/json',
  txt: `text/plain${TEXT}`,
  md: `text/markdown${TEXT}`,
  xml: 'application/xml',
  csv: `text/csv${TEXT}`,
  svg: 'image/svg+xml',
  png: 'image/png',
  gif: 'image/gif',
  webp: 'image/webp',
  avif: 'image/avif',
  bmp: 'image/bmp',
  jpg: 'image/jpeg',
  jpeg: 'image/jpeg',
  ico: 'image/x-icon',
  mp4: 'video/mp4',
  m4v: 'video/mp4',
  webm: 'video/webm',
  ogv: 'video/ogg',
  mp3: 'audio/mpeg',
  m4a: 'audio/mp4',
  ogg: 'audio/ogg',
  oga: 'audio/ogg',
  opus: 'audio/ogg',
  wav: 'audio/wav',
  flac: 'audio/flac',
  pdf: 'application/pdf',
  woff: 'font/woff',
  woff2: 'font/woff2',
  ttf: 'font/ttf',
  otf: 'font/otf',
  wasm: 'application/wasm',
  zip: 'application/zip',
};

export const OCTETS = 'application/octet-stream';

// RFC 9110 §5.6.2 (token), §5.6.4 (quoted-string), §8.3.1 (media-type).
const TOKEN = "[!#$%&'*+\\-.^_`|~0-9A-Za-z]+";
const QUOTED =
  '"(?:[\\t \\x21\\x23-\\x5B\\x5D-\\x7E\\x80-\\xFF]|\\\\[\\t \\x21-\\x7E\\x80-\\xFF])*"';
const MEDIA_TYPE = new RegExp(
  `^${TOKEN}/${TOKEN}(?:[ \\t]*;[ \\t]*${TOKEN}=(?:${TOKEN}|${QUOTED}))*$`,
);

/** Whether `s` is a media type: `type "/" subtype *( OWS ";" OWS parameter )`. */
export function validMediaType(s: string): boolean {
  return MEDIA_TYPE.test(s);
}

/** The `Content-Type` of a file named `name` with xattr `xattr` (sw.md §5). */
export function contentType(name: string, xattr?: string): string {
  if (xattr !== undefined && validMediaType(xattr)) return xattr;
  const dot = name.lastIndexOf('.');
  if (dot < 0) return OCTETS;
  return BY_EXT[name.slice(dot + 1).toLowerCase()] ?? OCTETS;
}
