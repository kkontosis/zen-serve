// IMAP (a subset of RFC 3501, with IDLE, RFC 2177, and LITERAL+, RFC 7888)
// as a relayed service's handler, over implicit TLS. One mailbox, INBOX:
// the user's `messages`. Users log in with app passwords. The session is
// the connection's state, so a successor continues it, a large FETCH
// included: its responses are written across steps (`again()`), each
// step's bytes with the commit of what they report. An idling session is
// woken (`wakeOn(user)`) when mail arrives or the mailbox changes
// elsewhere (`wakeOnMail`), and tells the client.
import type { DbTransaction } from '@zen/db';
import type { RawHandler, RawStep } from '@zen/svc';
import { headerLines } from './header.js';
import {
  type FetchAtt,
  frame,
  ImapError,
  inSet,
  internalDate,
  matches,
  parseFetchAtts,
  parseSearch,
  patternMatches,
  Reader,
  type SeqSet,
  seqOf,
  setMax,
  toView,
  type View,
  viewAppend,
  viewCount,
  viewMax,
  viewRemove,
  viewUids,
} from './imapparse.js';
import { type AppPassword, type Counter, type Message, validityOf } from './schema.js';
import {
  bodyRange,
  checkAppPassword,
  expunge,
  type FlagOp,
  headerLength,
  loginUser,
  SYSTEM_FLAGS,
  setFlags,
} from './store.js';

/** What the server offers (no AUTHENTICATE mechanisms: LOGIN with app passwords). */
export const CAPABILITIES = 'IMAP4rev1 LITERAL+ IDLE';

/** The longest command, its literals included. */
export const MAX_COMMAND = 65_536;

/** The most a FETCH writes in one step. */
const FETCH_STEP = 256 * 1024;

/** Room kept in a step for the replies after a FETCH's bytes. */
const RESERVE = 8192;

/** A session's state may not pass this (svc.md's state budget holds the app's too). */
const MAX_STATE = 32 * 1024;

/** Options of the handler. */
export interface ImapOptions {
  /** Tests: runs inside the transaction of each step that continues a FETCH. */
  onFetchMore?: () => Promise<void>;
}

/** The selected mailbox. */
interface Selected {
  /** EXAMINE: read only. */
  ro: boolean;
  /** The uids the client knows, as ranges: sequence numbers are positions in it. */
  view: View;
  /** The change counter the client has been told of. */
  modseq: number;
  /** Change counters of the session's own changes since: not told again. */
  own: [number, number][];
}

/** A FETCH being written. */
interface Pending {
  tag: string;
  /** `FETCH` or `UID FETCH`. */
  name: string;
  atts: FetchAtt[];
  /** The uids left. */
  rest: View;
  /** The message being written: its flags (fixed when it began) and bytes written so far. */
  cur?: { uid: number; seq: number; flags: string[]; at: number; seen: boolean };
}

/** The session, as the connection's state. */
interface Session {
  greeted?: boolean;
  user?: string;
  /** The id of the app password it logged in with. */
  pw?: string;
  fails?: number;
  sel?: Selected;
  /** IDLE's tag, while idling. */
  idle?: string;
  /** Continuations sent for the command at the input's start. */
  cont?: number;
  out?: Pending;
}

const enc = new TextEncoder();
const dec = new TextDecoder();

/** A piece of a FETCH response: bytes, or a range of the stored message. */
type Piece = { b: Uint8Array } | { r: [number, number] };

const pieceLen = (p: Piece) => ('b' in p ? p.b.length : p.r[1]);

const flagList = (f: string[]) => `(${f.join(' ')})`;

const canonFlag = new Map(SYSTEM_FLAGS.map((f) => [f.toLowerCase(), f]));

/** The handler of an IMAP service. */
export function imap(opts: ImapOptions = {}): RawHandler {
  return async (c, tx) => {
    const s: Session = c.state ? JSON.parse(dec.decode(c.state)) : {};
    const turn = new Turn(c, tx, s, opts);
    await turn.run();
    c.wakeOn(...(s.idle && s.user ? [s.user] : []));
    const state = enc.encode(JSON.stringify(s));
    if (state.length > MAX_STATE && !turn.closed) {
      // A very fragmented mailbox (TD-MAIL-SCOPE).
      c.write('* BYE [ALERT] The session is too large; select the mailbox again\r\n');
      c.close();
    }
    c.state = state;
  };
}

/** One call of the handler. */
class Turn {
  closed = false;

  constructor(
    private readonly c: RawStep,
    private readonly tx: DbTransaction,
    private readonly s: Session,
    private readonly opts: ImapOptions,
  ) {}

  private send(text: string): void {
    this.c.write(text);
  }

  private close(): void {
    this.c.close();
    this.closed = true;
  }

  async run(): Promise<void> {
    const { c, s } = this;
    if (!s.greeted) {
      this.send(`* OK [CAPABILITY ${CAPABILITIES}] zen IMAP ready\r\n`);
      s.greeted = true;
    }
    if (s.user) {
      const pw = await this.tx.table<AppPassword>('app_passwords').get([s.user, s.pw!]);
      if (!pw) {
        this.send('* BYE The app password was revoked\r\n');
        this.close();
        c.ack(c.input.length);
        return;
      }
    }
    if (s.out) {
      // No command is read until the FETCH's responses are out.
      await this.opts.onFetchMore?.();
      if (!(await this.fetchMore())) {
        c.again();
        return;
      }
    }
    if (s.idle && s.sel) await this.sync();
    const input = c.input;
    let at = 0;
    while (!this.closed) {
      if (c.room < RESERVE * 2) {
        c.again();
        break;
      }
      const f = frame(input.subarray(at), MAX_COMMAND);
      if (f.kind === 'need') {
        const sent = s.cont ?? 0;
        if (f.syncs > sent) {
          this.send('+ Ready for literal\r\n'.repeat(f.syncs - sent));
          s.cont = f.syncs;
        }
        break;
      }
      if (f.kind === 'too_long') {
        if (f.at < 0) {
          this.send('* BYE Command too long\r\n');
          this.close();
          at = input.length;
          break;
        }
        // Refused before the client sent the literal.
        let tag = '*';
        try {
          tag = new Reader(input.subarray(at)).tag();
        } catch {}
        this.send(`${tag} BAD Command too long\r\n`);
        at += f.at;
        s.cont = 0;
        continue;
      }
      const cmd = input.subarray(at, at + f.end);
      at += f.end;
      s.cont = 0;
      if (s.idle) this.done(cmd);
      else await this.command(cmd);
      if (s.out) {
        c.again();
        break;
      }
    }
    if (c.eof && !this.closed) {
      // The client went away.
      this.close();
      at = input.length;
    }
    c.ack(at);
  }

  /** A line while idling: DONE ends the IDLE; anything else is refused and ends it too. */
  private done(cmd: Uint8Array): void {
    const tag = this.s.idle!;
    delete this.s.idle;
    if (dec.decode(cmd).trim().toUpperCase() === 'DONE') this.send(`${tag} OK IDLE terminated\r\n`);
    else this.send(`${tag} BAD Expected DONE\r\n`);
  }

  private async command(cmd: Uint8Array): Promise<void> {
    const r = new Reader(cmd);
    let tag: string;
    try {
      tag = r.tag();
    } catch {
      this.send('* BAD Missing tag\r\n');
      return;
    }
    try {
      r.sp();
      await this.dispatch(tag, r.atom().toUpperCase(), r);
    } catch (e) {
      if (!(e instanceof ImapError)) throw e;
      this.send(`${tag} ${e.kind} ${e.code ? `[${e.code}] ` : ''}${e.message}\r\n`);
    }
  }

  private async dispatch(tag: string, name: string, r: Reader): Promise<void> {
    const s = this.s;
    switch (name) {
      case 'CAPABILITY':
        r.end();
        this.send(`* CAPABILITY ${CAPABILITIES}\r\n${tag} OK CAPABILITY completed\r\n`);
        return;
      case 'NOOP':
      case 'CHECK':
        r.end();
        if (name === 'CHECK') this.selected();
        if (s.sel) await this.sync();
        this.send(`${tag} OK ${name} completed\r\n`);
        return;
      case 'LOGOUT':
        r.end();
        this.send(`* BYE Logging out\r\n${tag} OK LOGOUT completed\r\n`);
        this.close();
        return;
      case 'LOGIN':
        return this.login(tag, r);
      case 'AUTHENTICATE':
        throw new ImapError('NO', 'Use LOGIN with an app password');
      case 'STARTTLS':
        throw new ImapError('BAD', 'The connection is TLS already');
    }
    if (!s.user) throw new ImapError('BAD', `${name} needs LOGIN first`);
    switch (name) {
      case 'SELECT':
      case 'EXAMINE':
        return this.select(tag, name, r);
      case 'LIST':
      case 'LSUB':
        return this.list(tag, name, r);
      case 'STATUS':
        return this.status(tag, r);
      case 'IDLE':
        r.end();
        s.idle = tag;
        this.send('+ idling\r\n');
        if (s.sel) await this.sync();
        return;
      case 'CREATE':
      case 'DELETE':
      case 'RENAME':
      case 'SUBSCRIBE':
      case 'UNSUBSCRIBE':
      case 'APPEND':
        throw new ImapError('NO', 'Only INBOX exists here', 'CANNOT');
    }
    const sel = this.selected();
    switch (name) {
      case 'CLOSE':
        r.end();
        if (!sel.ro) await this.expungeDeleted(false);
        delete s.sel;
        this.send(`${tag} OK CLOSE completed\r\n`);
        return;
      case 'EXPUNGE':
        r.end();
        if (sel.ro) throw new ImapError('NO', 'The mailbox is read only', 'READ-ONLY');
        await this.expungeDeleted(true);
        this.send(`${tag} OK EXPUNGE completed\r\n`);
        return;
      case 'FETCH':
      case 'STORE':
      case 'SEARCH':
        return this.selectedCommand(tag, name, r, false);
      case 'UID': {
        r.sp();
        const sub = r.atom().toUpperCase();
        if (sub === 'FETCH' || sub === 'STORE' || sub === 'SEARCH') {
          return this.selectedCommand(tag, sub, r, true);
        }
        if (sub === 'COPY') throw new ImapError('NO', 'Only INBOX exists here', 'CANNOT');
        throw new ImapError('BAD', `Unknown command UID ${sub}`);
      }
      case 'COPY':
        throw new ImapError('NO', 'Only INBOX exists here', 'CANNOT');
      default:
        throw new ImapError('BAD', `Unknown command ${name}`);
    }
  }

  private selected(): Selected {
    if (!this.s.sel) throw new ImapError('BAD', 'No mailbox selected');
    return this.s.sel;
  }

  private selectedCommand(tag: string, name: string, r: Reader, uid: boolean): Promise<void> {
    if (name === 'FETCH') return this.fetch(tag, r, uid);
    if (name === 'STORE') return this.store(tag, r, uid);
    return this.search(tag, r, uid);
  }

  // ---------------------------------------------------------------- not selected

  private async login(tag: string, r: Reader): Promise<void> {
    const s = this.s;
    if (s.user) throw new ImapError('BAD', 'Logged in already');
    r.sp();
    const login = r.astring();
    r.sp();
    const password = r.astring();
    r.end();
    const user = await loginUser(this.tx, login);
    const id = await checkAppPassword(this.tx, user, password);
    if (!id) {
      s.fails = (s.fails ?? 0) + 1;
      this.send(`${tag} NO [AUTHENTICATIONFAILED] Authentication failed\r\n`);
      if (s.fails >= 3) {
        this.send('* BYE Too many authentication failures\r\n');
        this.close();
      }
      return;
    }
    s.user = user;
    s.pw = id;
    delete s.fails;
    this.send(`${tag} OK [CAPABILITY ${CAPABILITIES}] Logged in\r\n`);
  }

  private rows(): Promise<Message[]> {
    return this.tx
      .table<Message>('messages')
      .query()
      .where('user', '=', this.s.user!)
      .orderBy('uid')
      .all();
  }

  private counter(): Promise<Counter | undefined> {
    return this.tx.table<Counter>('counters').get(this.s.user!);
  }

  private mailbox(r: Reader): void {
    const box = r.astring();
    if (box.toUpperCase() !== 'INBOX')
      throw new ImapError('NO', `No mailbox ${box}`, 'NONEXISTENT');
  }

  private async select(tag: string, name: string, r: Reader): Promise<void> {
    const s = this.s;
    // A failed SELECT leaves no mailbox selected.
    delete s.sel;
    r.sp();
    this.mailbox(r);
    r.end();
    const rows = await this.rows();
    const ctr = await this.counter();
    const ro = name === 'EXAMINE';
    s.sel = { ro, view: toView(rows.map((m) => m.uid)), modseq: ctr?.modseq ?? 0, own: [] };
    const unseen = rows.findIndex((m) => !m.flags.includes('\\Seen'));
    const flags = flagList(SYSTEM_FLAGS);
    this.send(
      `* FLAGS ${flags}\r\n` +
        `* OK [PERMANENTFLAGS ${ro ? '()' : flags}] Flags\r\n` +
        `* ${rows.length} EXISTS\r\n* 0 RECENT\r\n` +
        (unseen >= 0 ? `* OK [UNSEEN ${unseen + 1}] First unseen\r\n` : '') +
        `* OK [UIDVALIDITY ${validityOf(ctr)}] UIDs valid\r\n` +
        `* OK [UIDNEXT ${ctr?.next ?? 1}] Predicted next UID\r\n` +
        `${tag} OK [${ro ? 'READ-ONLY' : 'READ-WRITE'}] ${name} completed\r\n`,
    );
  }

  private list(tag: string, name: string, r: Reader): void {
    r.sp();
    const ref = r.astring();
    r.sp();
    const pattern = r.pattern();
    r.end();
    if (name === 'LIST' && pattern === '') this.send('* LIST (\\Noselect) "/" ""\r\n');
    else if (patternMatches(ref, pattern, 'INBOX')) {
      this.send(`* ${name} (${name === 'LIST' ? '\\HasNoChildren' : ''}) "/" INBOX\r\n`);
    }
    this.send(`${tag} OK ${name} completed\r\n`);
  }

  private async status(tag: string, r: Reader): Promise<void> {
    r.sp();
    this.mailbox(r);
    r.sp();
    const items = r.list(() => r.atom().toUpperCase());
    r.end();
    const rows = await this.rows();
    const ctr = await this.counter();
    const out = items.map((i) => {
      switch (i) {
        case 'MESSAGES':
          return `MESSAGES ${rows.length}`;
        case 'RECENT':
          return 'RECENT 0';
        case 'UIDNEXT':
          return `UIDNEXT ${ctr?.next ?? 1}`;
        case 'UIDVALIDITY':
          return `UIDVALIDITY ${validityOf(ctr)}`;
        case 'UNSEEN':
          return `UNSEEN ${rows.filter((m) => !m.flags.includes('\\Seen')).length}`;
        default:
          throw new ImapError('BAD', `Unknown status item ${i}`);
      }
    });
    this.send(`* STATUS INBOX (${out.join(' ')})\r\n${tag} OK STATUS completed\r\n`);
  }

  // ---------------------------------------------------------------- updates

  /**
   * Tell the client what changed since it was last told (RFC 3501 §7.4.1:
   * where EXPUNGE may be sent): expunges, from the highest sequence number
   * down; new messages; and flags changed elsewhere.
   */
  private async sync(): Promise<void> {
    const sel = this.s.sel!;
    const rows = await this.rows();
    const ctr = await this.counter();
    const byUid = new Map(rows.map((m) => [m.uid, m]));
    const known = viewMax(sel.view);
    let out = '';
    const gone = viewUids(sel.view).filter((u) => !byUid.has(u));
    for (const u of gone.reverse()) {
      out += `* ${seqOf(sel.view, u)} EXPUNGE\r\n`;
      sel.view = viewRemove(sel.view, u);
    }
    for (const m of rows) {
      const ms = m.modseq ?? 0;
      if (m.uid > known || ms <= sel.modseq || sel.own.some(([a, b]) => ms >= a && ms <= b)) {
        continue;
      }
      const seq = seqOf(sel.view, m.uid);
      if (seq) out += `* ${seq} FETCH (UID ${m.uid} FLAGS ${flagList(m.flags)})\r\n`;
    }
    const fresh = rows.filter((m) => m.uid > known).map((m) => m.uid);
    if (fresh.length) {
      sel.view = viewAppend(sel.view, fresh);
      out += `* ${viewCount(sel.view)} EXISTS\r\n`;
    }
    sel.modseq = Math.max(sel.modseq, ctr?.modseq ?? 0);
    sel.own = [];
    if (out) this.send(out);
  }

  /** Note the change counters of the session's own change (consecutive in one transaction). */
  private own(modseqs: number[]): void {
    if (!modseqs.length) return;
    const own = this.s.sel!.own;
    own.push([Math.min(...modseqs), Math.max(...modseqs)]);
    // Bounded: a change it forgets is told again, which does no harm.
    if (own.length > 64) own.shift();
  }

  // ---------------------------------------------------------------- sets

  /** The uids a set names: sequence numbers or uids in the view, in order. */
  private uidsOf(set: SeqSet, uid: boolean): number[] {
    const view = this.s.sel!.view;
    const all = viewUids(view);
    if (uid) return all.filter((u) => inSet(set, u, viewMax(view)));
    const max = setMax(
      set.map(([a, b]) => [a === '*' ? 1 : a, b === '*' ? 1 : b]),
      all.length,
    );
    if (max > all.length) throw new ImapError('BAD', 'No such message');
    return all.filter((_, i) => inSet(set, i + 1, all.length));
  }

  // ---------------------------------------------------------------- FETCH

  private async fetch(tag: string, r: Reader, uid: boolean): Promise<void> {
    r.sp();
    const set = r.seqSet();
    r.sp();
    const atts = parseFetchAtts(r);
    r.end();
    if (uid && !atts.some((a) => a.kind === 'uid')) atts.unshift({ kind: 'uid' });
    this.s.out = {
      tag,
      name: uid ? 'UID FETCH' : 'FETCH',
      atts,
      rest: toView(this.uidsOf(set, uid)),
    };
    await this.fetchMore();
  }

  /**
   * Write the pending FETCH's responses, up to what this step may hold.
   * True when they are all out (and the tagged OK with them).
   */
  private async fetchMore(): Promise<boolean> {
    const s = this.s;
    const out = s.out!;
    const sel = s.sel!;
    const table = this.tx.table<Message>('messages');
    let budget = Math.min(FETCH_STEP, this.c.room - RESERVE);
    while (budget > 0) {
      let m: Message | undefined;
      if (!out.cur) {
        const uid = out.rest[0]?.[0];
        if (uid === undefined) {
          this.send(`${out.tag} OK ${out.name} completed\r\n`);
          delete s.out;
          return true;
        }
        out.rest = viewRemove(out.rest, uid);
        m = await table.get([s.user!, uid]);
        // Expunged elsewhere since: nothing to tell (RFC 3501 §7.4.1).
        if (!m) continue;
        let flags = m.flags;
        let seen = false;
        const marks = out.atts.some((a) => a.kind === 'body' && !a.peek);
        if (marks && !sel.ro && !flags.includes('\\Seen')) {
          const ch = await setFlags(this.tx, s.user!, [uid], 'add', ['\\Seen']);
          this.own(ch.map((x) => x.modseq));
          flags = ch[0]?.flags ?? flags;
          // The client learns of the new flag in this response.
          seen = !out.atts.some((a) => a.kind === 'flags');
        }
        out.cur = { uid, seq: seqOf(sel.view, uid), flags, at: 0, seen };
      } else {
        m = await table.get([s.user!, out.cur.uid]);
        if (!m) {
          // Expunged elsewhere in the middle of its literal: the response
          // can't be finished.
          this.c.abort();
          this.closed = true;
          delete s.out;
          return true;
        }
      }
      const cur = out.cur;
      const pieces = await this.pieces(m, cur, out.atts);
      let pos = 0;
      for (const p of pieces) {
        const len = pieceLen(p);
        if (pos + len <= cur.at || budget <= 0) {
          pos += len;
          continue;
        }
        const skip = cur.at - pos;
        const n = Math.min(len - skip, budget);
        const bytes =
          'b' in p ? p.b.subarray(skip, skip + n) : await bodyRange(this.tx, m, p.r[0] + skip, n);
        this.c.write(bytes);
        cur.at += n;
        budget -= n;
        pos += len;
      }
      if (cur.at < pos) return false;
      delete out.cur;
    }
    return false;
  }

  /** A message's FETCH response, as pieces. */
  private async pieces(m: Message, cur: NonNullable<Pending['cur']>, atts: FetchAtt[]) {
    const out: Piece[] = [];
    let text = `* ${cur.seq} FETCH (`;
    const flush = () => {
      if (text) out.push({ b: enc.encode(text) });
      text = '';
    };
    let hdr: number | undefined;
    const header = async () => {
      hdr ??= await headerLength(this.tx, m);
      return hdr;
    };
    for (const [i, a] of atts.entries()) {
      if (i > 0) text += ' ';
      switch (a.kind) {
        case 'uid':
          text += `UID ${m.uid}`;
          break;
        case 'flags':
          text += `FLAGS ${flagList(cur.flags)}`;
          break;
        case 'internaldate':
          text += `INTERNALDATE ${internalDate(m.received)}`;
          break;
        case 'size':
          text += `RFC822.SIZE ${m.size}`;
          break;
        case 'body': {
          // The section: a range of the stored message, or computed bytes.
          let range: [number, number] | undefined;
          let bytes: Uint8Array | undefined;
          const sec = a.section;
          if (sec.kind === 'all') range = [0, m.size];
          else if (sec.kind === 'header') range = [0, await header()];
          else if (sec.kind === 'text') {
            const h = await header();
            range = [h, m.size - h];
          } else {
            const names = new Set(sec.names.map((n) => n.toLowerCase()));
            const lines = headerLines(await bodyRange(this.tx, m, 0, await header()));
            const keep = lines.filter((l) => names.has(l.name) !== sec.not);
            bytes = new Uint8Array(keep.reduce((n, l) => n + l.raw.length, 0) + 2);
            let at = 0;
            for (const l of keep) {
              bytes.set(l.raw, at);
              at += l.raw.length;
            }
            bytes.set([13, 10], at);
          }
          let len = range ? range[1] : bytes!.length;
          let from = 0;
          if (a.partial) {
            from = Math.min(a.partial[0], len);
            len = Math.min(a.partial[1], len - from);
          }
          text += `${a.name}${a.partial ? `<${a.partial[0]}>` : ''} {${len}}\r\n`;
          flush();
          if (len > 0) {
            if (range) out.push({ r: [range[0] + from, len] });
            else out.push({ b: bytes!.subarray(from, from + len) });
          }
          break;
        }
      }
    }
    if (cur.seen) text += `${atts.length ? ' ' : ''}FLAGS ${flagList(cur.flags)}`;
    text += ')\r\n';
    flush();
    return out;
  }

  // ---------------------------------------------------------------- STORE

  private async store(tag: string, r: Reader, uid: boolean): Promise<void> {
    const sel = this.s.sel!;
    r.sp();
    const set = r.seqSet();
    r.sp();
    const item = r.atom().toUpperCase();
    const m = /^([+-]?)FLAGS(\.SILENT)?$/.exec(item);
    if (!m) throw new ImapError('BAD', `Unknown STORE item ${item}`);
    r.sp();
    const raw = r.peek() === 40 ? r.list(() => r.flag()) : [r.flag()];
    while (r.maybeSp()) raw.push(r.flag());
    r.end();
    const flags = raw.map((f) => {
      const c = canonFlag.get(f.toLowerCase());
      if (!c) throw new ImapError('BAD', `Flag ${f} can't be stored (no keywords)`);
      return c;
    });
    if (sel.ro) throw new ImapError('NO', 'The mailbox is read only', 'READ-ONLY');
    const uids = this.uidsOf(set, uid);
    const op: FlagOp = m[1] === '+' ? 'add' : m[1] === '-' ? 'remove' : 'set';
    const changed = await setFlags(this.tx, this.s.user!, uids, op, flags);
    this.own(changed.map((x) => x.modseq));
    let out = '';
    if (!m[2]) {
      const now = new Map(changed.map((x) => [x.uid, x.flags]));
      const table = this.tx.table<Message>('messages');
      for (const u of uids) {
        const f = now.get(u) ?? (await table.get([this.s.user!, u]))?.flags;
        if (!f) continue;
        out += `* ${seqOf(sel.view, u)} FETCH (FLAGS ${flagList(f)}${uid ? ` UID ${u}` : ''})\r\n`;
      }
    }
    this.send(`${out}${tag} OK ${uid ? 'UID ' : ''}STORE completed\r\n`);
  }

  // ---------------------------------------------------------------- SEARCH

  private async search(tag: string, r: Reader, uid: boolean): Promise<void> {
    const sel = this.s.sel!;
    r.sp();
    const key = parseSearch(r);
    r.end();
    const byUid = new Map((await this.rows()).map((m) => [m.uid, m]));
    const uids = viewUids(sel.view);
    const scope = { maxSeq: uids.length, maxUid: viewMax(sel.view) };
    const found: number[] = [];
    for (const [i, u] of uids.entries()) {
      const m = byUid.get(u);
      if (!m) continue;
      const f = m.fields;
      const hit = matches(
        key,
        {
          seq: i + 1,
          uid: u,
          flags: m.flags,
          size: m.size,
          received: m.received,
          fields: {
            from: f?.from ?? m.from,
            to: f?.to ?? m.to,
            cc: f?.cc ?? '',
            subject: f?.subject ?? '',
          },
        },
        scope,
      );
      if (hit) found.push(uid ? u : i + 1);
    }
    this.send(
      `* SEARCH${found.map((n) => ` ${n}`).join('')}\r\n${tag} OK ${uid ? 'UID ' : ''}SEARCH completed\r\n`,
    );
  }

  // ---------------------------------------------------------------- EXPUNGE

  /** Expunge the messages marked `\Deleted`; with `tell`, send their EXPUNGE responses. */
  private async expungeDeleted(tell: boolean): Promise<void> {
    const sel = this.s.sel!;
    const inView = new Set(viewUids(sel.view));
    const deleted = (await this.rows())
      .filter((m) => inView.has(m.uid) && m.flags.includes('\\Deleted'))
      .map((m) => m.uid);
    const gone = await expunge(this.tx, this.s.user!, deleted);
    let out = '';
    for (const u of gone.sort((a, b) => b - a)) {
      out += `* ${seqOf(sel.view, u)} EXPUNGE\r\n`;
      sel.view = viewRemove(sel.view, u);
    }
    if (tell && out) this.send(out);
  }
}

export type { FetchAtt } from './imapparse.js';
