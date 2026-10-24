// Inbound SMTP (RFC 5321) as a relayed service's handler, with STARTTLS
// (RFC 3207), SIZE (RFC 1870), PIPELINING and 8BITMIME. A message is
// staged as `parts` rows while DATA arrives, each step's bytes committed
// with its ack; at the end one commit stores a `messages` row for each
// recipient, deletes the draft, emits `mail/received` and carries the
// `250` reply, so a handler killed at any point never loses or doubles a
// message: its successor continues from the last committed step.
import { bytes } from '@zen/client';
import type { DbTransaction } from '@zen/db';
import { LineReader, type RawHandler, type RawStep } from '@zen/svc';
import { parseFields, scanHeaderEnd } from './header.js';
import {
  type Counter,
  type Draft,
  type HeaderFields,
  type Mailbox,
  type Message,
  newValidity,
  PART_BYTES,
  type Part,
  type Received,
  receivedTopic,
} from './schema.js';

/** The largest message accepted, in bytes (`SIZE`). */
export const MAX_SIZE = 10_485_760;
/** The most recipients of one message. */
export const MAX_RCPTS = 100;
/** The longest command line, with its CRLF. */
export const MAX_LINE = 1000;

/** Options of the handler. */
export interface SmtpOptions {
  /** The server's name, in the greeting and the `Received:` header. */
  host: string;
  /** Offer STARTTLS (the service is served with `starttls()`; default true). */
  starttls?: boolean;
  /** The largest message (default `MAX_SIZE`). */
  maxSize?: number;
  /** Tests: run inside the transaction at the end of DATA, before the message is stored. */
  onDataEnd?: () => Promise<void>;
}

// The DATA decoder (RFC 5321 §4.5.2): dot-unstuffing, and the end of the
// message at CRLF "." CRLF, across steps. Its state is where the last
// byte left it. Only CRLF ends a line: a bare LF never starts one, so
// "\n.\n" isn't an end (no SMTP smuggling).
/** At the start of a line. */
const D_LINE = 0;
/** Inside a line. */
const D_MID = 1;
/** Inside a line, after CR. */
const D_CR = 2;
/** A line that began with a dot, held. */
const D_DOT = 3;
/** That dot, then CR, held. */
const D_DOTCR = 4;

/** What `decodeData` made of some DATA input. */
export interface Decoded {
  /** The message bytes it gave. */
  out: Uint8Array;
  /** Input bytes used: all of it, or up to the end of the message. */
  used: number;
  /** The decoder's state after them. */
  state: number;
  /** The message ended (its terminator was used). */
  end: boolean;
}

/** Decode DATA input from `state` (`D_LINE` right after the `354`). */
export function decodeData(state: number, input: Uint8Array): Decoded {
  const out = new Uint8Array(input.length + 1);
  let o = 0;
  let st = state;
  const mid = (b: number) => {
    out[o++] = b;
    st = b === 13 ? D_CR : D_MID;
  };
  for (let i = 0; i < input.length; i++) {
    const b = input[i]!;
    switch (st) {
      case D_LINE:
        if (b === 46) st = D_DOT;
        else mid(b);
        break;
      case D_MID:
        mid(b);
        break;
      case D_CR:
        out[o++] = b;
        st = b === 10 ? D_LINE : b === 13 ? D_CR : D_MID;
        break;
      case D_DOT:
        // A dot that starts a line is dropped (it was stuffed).
        if (b === 13) st = D_DOTCR;
        else mid(b);
        break;
      default:
        if (b === 10) return { out: out.subarray(0, o), used: i + 1, state: D_LINE, end: true };
        out[o++] = 13;
        st = D_CR;
        out[o++] = b;
        st = b === 13 ? D_CR : D_MID;
    }
  }
  return { out: out.subarray(0, o), used: input.length, state: st, end: false };
}

/** A recipient accepted by `RCPT`. */
interface Rcpt {
  addr: string;
  user: string;
}

/** A message being received. */
interface Data {
  draft: string;
  /** The part being filled, and its bytes. */
  seq: number;
  len: number;
  /** Body bytes so far. */
  size: number;
  /** The decoder's state. */
  dec: number;
  /** Over the size limit: the rest is read and dropped. */
  over: boolean;
  /** Swept while it arrived (`sweepDrafts`): the rest is read and dropped. */
  lost?: boolean;
  /** The header's length in the body, through its blank line (-1: not ended yet). */
  hdr?: number;
  /** How much of CRLF CRLF the last body bytes matched (`scanHeaderEnd`). */
  hm?: number;
}

/** The session, as the connection's state. */
interface Session {
  /** The `220` was sent. */
  greeted?: boolean;
  /** The client's `EHLO`/`HELO` name (none after STARTTLS until it says it again). */
  helo?: string;
  esmtp?: boolean;
  /** The reverse-path of the open transaction ('' for `<>`). */
  from?: string;
  rcpt?: Rcpt[];
  data?: Data;
}

const reply = (code: number, text: string) => `${code} ${text}\r\n`;

/** The handler of an inbound SMTP service. */
export function smtp(opts: SmtpOptions): RawHandler {
  const host = opts.host;
  const offerTls = opts.starttls ?? true;
  const maxSize = opts.maxSize ?? MAX_SIZE;
  const text = new TextDecoder();

  return async (c, tx) => {
    const s: Session = c.state ? JSON.parse(text.decode(c.state)) : {};
    if (!s.greeted) {
      c.write(reply(220, `${host} ESMTP zen`));
      s.greeted = true;
    }
    const input = c.input;
    let at = 0;
    let done = false;
    while (!done) {
      if (s.data) {
        const d = s.data;
        const r = decodeData(d.dec, input.subarray(at));
        at += r.used;
        d.dec = r.state;
        await stage(tx, d, r.out, maxSize);
        if (!r.end) break;
        if (opts.onDataEnd) await opts.onDataEnd();
        c.write(await store(tx, c, s, d, host));
        delete s.data;
        delete s.from;
        delete s.rcpt;
        continue;
      }
      const lr = new LineReader(input.subarray(at), { max: MAX_LINE });
      const line = lr.nextText();
      if (line === null) {
        if (lr.tooLong) {
          c.write(reply(500, '5.5.2 Line too long'));
          c.close();
          at = input.length;
          done = true;
        }
        break;
      }
      at += lr.consumed;
      done = await command(tx, c, s, line);
    }
    if (c.eof && !done) {
      // The client went away: drop what it was sending, and close.
      if (s.data) await dropDraft(tx, s.data, true);
      delete s.data;
      at = input.length;
      c.close();
    }
    c.ack(at);
    c.state = bytes.utf8(JSON.stringify(s));
  };

  /** Answer one command line; true when the step must stop reading (QUIT, STARTTLS, close). */
  async function command(
    tx: DbTransaction,
    c: RawStep,
    s: Session,
    line: string,
  ): Promise<boolean> {
    const sp = line.indexOf(' ');
    const verb = (sp < 0 ? line : line.slice(0, sp)).toUpperCase();
    const arg = sp < 0 ? '' : line.slice(sp + 1).trim();
    const reset = () => {
      delete s.from;
      delete s.rcpt;
    };
    switch (verb) {
      case 'EHLO':
      case 'HELO': {
        if (!arg) {
          c.write(reply(501, `5.5.4 ${verb} needs a domain`));
          return false;
        }
        reset();
        s.helo = arg;
        s.esmtp = verb === 'EHLO';
        if (verb === 'HELO') {
          c.write(reply(250, host));
          return false;
        }
        const ext = ['PIPELINING', `SIZE ${maxSize}`, '8BITMIME'];
        if (offerTls && !c.tls) ext.push('STARTTLS');
        const lines = [`${host} greets ${arg}`, ...ext];
        c.write(lines.map((l, i) => `250${i === lines.length - 1 ? ' ' : '-'}${l}\r\n`).join(''));
        return false;
      }
      case 'STARTTLS': {
        if (!offerTls || c.tls) {
          c.write(reply(c.tls ? 503 : 502, '5.5.1 TLS not available'));
          return false;
        }
        if (arg) {
          c.write(reply(501, '5.5.4 No parameters allowed'));
          return false;
        }
        c.write(reply(220, '2.0.0 Ready to start TLS'));
        c.startTls();
        // RFC 3207 §4.2: the session starts over.
        reset();
        delete s.helo;
        delete s.esmtp;
        return true;
      }
      case 'MAIL': {
        if (!s.helo) {
          c.write(reply(503, '5.5.1 Say EHLO first'));
          return false;
        }
        if (s.from !== undefined) {
          c.write(reply(503, '5.5.1 Nested MAIL command'));
          return false;
        }
        const m = /^FROM:\s*<([^<>]*)>(?:\s+(.*))?$/i.exec(arg);
        if (!m) {
          c.write(reply(501, '5.5.4 Syntax: MAIL FROM:<address>'));
          return false;
        }
        for (const p of (m[2] ?? '').split(/\s+/).filter(Boolean)) {
          const [k, v] = p.split('=', 2) as [string, string | undefined];
          const key = k.toUpperCase();
          if (key === 'SIZE' && v !== undefined && /^\d+$/.test(v)) {
            if (Number(v) > maxSize) {
              c.write(reply(552, '5.3.4 Message size exceeds fixed limit'));
              return false;
            }
          } else if (key === 'BODY' && /^(7BIT|8BITMIME)$/i.test(v ?? '')) {
            // Both are stored as they come.
          } else {
            c.write(reply(555, `5.5.4 Unsupported parameter ${k}`));
            return false;
          }
        }
        s.from = path(m[1]!);
        s.rcpt = [];
        c.write(reply(250, '2.1.0 Ok'));
        return false;
      }
      case 'RCPT': {
        if (s.from === undefined) {
          c.write(reply(503, '5.5.1 Need MAIL first'));
          return false;
        }
        const m = /^TO:\s*<([^<>]+)>$/i.exec(arg);
        if (!m) {
          c.write(reply(501, '5.5.4 Syntax: RCPT TO:<address>'));
          return false;
        }
        if (s.rcpt!.length >= MAX_RCPTS) {
          c.write(reply(452, '4.5.3 Too many recipients'));
          return false;
        }
        const addr = path(m[1]!).toLowerCase();
        const box = await tx.table<Mailbox>('mailboxes').get(addr);
        if (!box) {
          c.write(reply(550, '5.1.1 No such user here'));
          return false;
        }
        s.rcpt!.push({ addr, user: box.user });
        c.write(reply(250, '2.1.5 Ok'));
        return false;
      }
      case 'DATA': {
        if (arg) {
          c.write(reply(501, '5.5.4 No parameters allowed'));
          return false;
        }
        if (s.from === undefined || !s.rcpt?.length) {
          c.write(reply(503, '5.5.1 Need RCPT first'));
          return false;
        }
        const draft = bytes.hex(crypto.getRandomValues(new Uint8Array(16)));
        await tx
          .table<Draft>('drafts')
          .insert({ draft, conn: bytes.hex(c.conn), started: Date.now() });
        s.data = { draft, seq: 0, len: 0, size: 0, dec: D_LINE, over: false, hdr: -1, hm: 2 };
        c.write(reply(354, 'End data with <CR><LF>.<CR><LF>'));
        return false;
      }
      case 'RSET':
        reset();
        c.write(reply(250, '2.0.0 Ok'));
        return false;
      case 'NOOP':
        c.write(reply(250, '2.0.0 Ok'));
        return false;
      case 'VRFY':
        c.write(reply(252, '2.5.0 Cannot VRFY user, but will accept the message'));
        return false;
      case 'QUIT':
        c.write(reply(221, '2.0.0 Bye'));
        c.close();
        return true;
      case 'BDAT':
      case 'EXPN':
      case 'HELP':
      case 'ETRN':
      case 'TURN':
      case 'AUTH':
        c.write(reply(502, '5.5.1 Command not implemented'));
        return false;
      default:
        c.write(reply(500, '5.5.2 Command unrecognized'));
        return false;
    }
  }
}

/** A path without a source route (RFC 5321 §4.1.2: `@a,@b:user@x`). */
function path(p: string): string {
  const t = p.trim();
  return t.startsWith('@') && t.includes(':') ? t.slice(t.indexOf(':') + 1) : t;
}

/** Append decoded body bytes to the draft's parts; past the limit, drop them all. */
async function stage(tx: DbTransaction, d: Data, data: Uint8Array, maxSize: number): Promise<void> {
  if (d.over || data.length === 0) return;
  if ((d.hdr ?? -1) < 0) {
    const h = scanHeaderEnd(d.hm ?? 2, data);
    d.hm = h.m;
    if (h.end >= 0) d.hdr = d.size + h.end;
  }
  d.size += data.length;
  if (d.size > maxSize) {
    d.over = true;
    await dropDraft(tx, d, false);
    return;
  }
  const parts = tx.table<Part>('parts');
  for (let at = 0; at < data.length; ) {
    if (d.len === PART_BYTES) {
      d.seq++;
      d.len = 0;
    }
    const take = Math.min(PART_BYTES - d.len, data.length - at);
    const piece = data.slice(at, at + take);
    let next = piece;
    if (d.len > 0) {
      const p = await parts.get([d.draft, d.seq]);
      if (!p) {
        d.over = true;
        d.lost = true;
        return;
      }
      next = new Uint8Array(d.len + take);
      next.set(p!.data);
      next.set(piece, d.len);
    }
    await parts.put({ draft: d.draft, seq: d.seq, data: next });
    d.len += take;
    at += take;
  }
}

/** Delete a draft's parts, and with `row` the draft itself. */
async function dropDraft(tx: DbTransaction, d: Data, row: boolean): Promise<void> {
  for (let seq = 0; seq <= d.seq; seq++) await tx.table<Part>('parts').delete([d.draft, seq]);
  d.seq = 0;
  d.len = 0;
  if (row) await tx.table<Draft>('drafts').delete(d.draft);
}

/** The end of DATA: store a copy for each recipient (or refuse it), and give the reply. */
async function store(
  tx: DbTransaction,
  c: RawStep,
  s: Session,
  d: Data,
  host: string,
): Promise<string> {
  const swept = !(await tx.table<Draft>('drafts').delete(d.draft));
  if (swept || d.lost) {
    await dropDraft(tx, d, false);
    return reply(451, '4.3.0 Message lost, try again');
  }
  if (d.over) return reply(552, '5.3.4 Message size exceeds fixed limit');
  const now = Date.now();
  const tls = c.tls ? ` (${c.tls.version}:${c.tls.suite})` : '';
  const proto = `${s.esmtp ? 'ESMTP' : 'SMTP'}${c.tls ? 'S' : ''}`;
  const peer = c.peer.addr ?? 'unknown';
  // Without a blank line, the whole message is its header.
  const hdr = (d.hdr ?? -1) >= 0 ? d.hdr! : d.size;
  const fields = await headerFields(tx, d.draft, hdr);
  for (const r of s.rcpt!) {
    const counters = tx.table<Counter>('counters');
    const ctr = await counters.get(r.user);
    const uid = ctr?.next ?? 1;
    const modseq = (ctr?.modseq ?? 0) + 1;
    const validity = ctr ? ctr.validity : newValidity();
    await counters.put({
      user: r.user,
      next: uid + 1,
      modseq,
      ...(validity === undefined ? {} : { validity }),
    });
    const head =
      `Received: from ${s.helo} ([${peer}])\r\n` +
      `\tby ${host} with ${proto}${tls} id ${d.draft}\r\n` +
      `\tfor <${r.addr}>; ${rfc5322Date(now)}\r\n`;
    await tx.table<Message>('messages').insert({
      user: r.user,
      uid,
      draft: d.draft,
      size: bytes.utf8(head).length + d.size,
      from: s.from ?? '',
      to: r.addr,
      received: now,
      head,
      flags: [],
      modseq,
      hdr: bytes.utf8(head).length + hdr,
      fields,
    });
    tx.emit(
      receivedTopic(tx.db),
      'message',
      { user: r.user, uid, size: d.size } satisfies Received,
      { key: r.user },
    );
  }
  return reply(250, `2.0.0 Ok: queued as ${d.draft}`);
}

/** The header's fields, when the header lies within the first part. */
async function headerFields(tx: DbTransaction, draft: string, hdr: number): Promise<HeaderFields> {
  const p =
    hdr > 0 && hdr <= PART_BYTES ? await tx.table<Part>('parts').get([draft, 0]) : undefined;
  return parseFields(p ? p.data.subarray(0, hdr) : new Uint8Array(0));
}

const DAYS = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

/** `Sat, 24 Oct 2026 12:00:00 +0000` (RFC 5322 §3.3), in UTC. */
function rfc5322Date(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, '0');
  return (
    `${DAYS[d.getUTCDay()]}, ${d.getUTCDate()} ${MONTHS[d.getUTCMonth()]} ${d.getUTCFullYear()} ` +
    `${p(d.getUTCHours())}:${p(d.getUTCMinutes())}:${p(d.getUTCSeconds())} +0000`
  );
}
