// A message's header section (RFC 5322 §2.2): where it ends, its fields,
// and the fields IMAP's HEADER.FIELDS picks. Bytes as stored; text is read
// as UTF-8 (8BITMIME), without RFC 2047 decoding.
import type { HeaderFields } from './schema.js';

/**
 * Where a header section ends, scanned as the message arrives: `m` is how
 * much of CRLF CRLF the last bytes matched (2 at the start, so a message
 * that begins with CRLF has an empty header). Returns the offset just
 * past the blank line within `data`, or -1, and the match after `data`.
 */
export function scanHeaderEnd(m: number, data: Uint8Array): { end: number; m: number } {
  for (let i = 0; i < data.length; i++) {
    const b = data[i]!;
    if (b === 13) m = m === 2 ? 3 : 1;
    else if (b === 10 && m === 3) return { end: i + 1, m: 4 };
    else if (b === 10 && m === 1) m = 2;
    else m = 0;
  }
  return { end: -1, m };
}

/** A header field: its name (lowercase) and its raw bytes, CRLF and folding included. */
export interface FieldLine {
  name: string;
  raw: Uint8Array;
}

/** Split a header section into its fields (the blank line, if there, isn't one). */
export function headerLines(hdr: Uint8Array): FieldLine[] {
  const out: FieldLine[] = [];
  const dec = new TextDecoder('latin1');
  let start = 0;
  let i = 0;
  const lineEnd = (from: number) => {
    const n = hdr.indexOf(10, from);
    return n < 0 ? hdr.length : n + 1;
  };
  while (i < hdr.length) {
    const end = lineEnd(i);
    // A line beginning with a space or tab continues the field before it.
    const next = end < hdr.length ? hdr[end]! : -1;
    if (next === 32 || next === 9) {
      i = end;
      continue;
    }
    const raw = hdr.subarray(start, end);
    const text = dec.decode(raw);
    if (text !== '\r\n' && text !== '\n') {
      const colon = text.indexOf(':');
      if (colon > 0) out.push({ name: text.slice(0, colon).trim().toLowerCase(), raw });
    }
    start = end;
    i = end;
  }
  return out;
}

const NAMES: Record<string, keyof HeaderFields> = {
  from: 'from',
  to: 'to',
  cc: 'cc',
  subject: 'subject',
  date: 'date',
  'message-id': 'messageId',
};

/** The fields a mailbox list shows and SEARCH looks at: the first of each, unfolded. */
export function parseFields(hdr: Uint8Array): HeaderFields {
  const f: HeaderFields = { from: '', to: '', cc: '', subject: '', date: '', messageId: '' };
  const seen = new Set<string>();
  const utf8 = new TextDecoder();
  for (const l of headerLines(hdr)) {
    const k = NAMES[l.name];
    if (!k || seen.has(k)) continue;
    seen.add(k);
    const text = utf8.decode(l.raw);
    f[k] = text
      .slice(text.indexOf(':') + 1)
      .replace(/\r?\n(?=[ \t])/g, '')
      .trim();
  }
  return f;
}
