// Lines of a text protocol (SMTP, IMAP, the echo service) out of a step's
// input: whole lines only, so a handler acks `consumed` and a partial line
// waits for the rest.

const text = new TextDecoder();

/** Options of a `LineReader`. */
export interface LineReaderOptions {
  /** The longest line accepted, with its terminator (default: no limit). */
  max?: number;
}

/**
 * Read the lines of `input`, each ended by CRLF or a bare LF:
 * ```ts
 * const r = new LineReader(c.input, { max: 1000 });
 * for (let l = r.nextText(); l !== null; l = r.nextText()) { … }
 * c.ack(r.consumed);
 * ```
 */
export class LineReader {
  readonly max: number;
  private at = 0;
  /** The next line has no terminator within `max` bytes: it is too long. */
  tooLong = false;

  constructor(
    readonly input: Uint8Array,
    opts: LineReaderOptions = {},
  ) {
    this.max = opts.max ?? Number.POSITIVE_INFINITY;
  }

  /** The next whole line, without its terminator; null when none is complete (or `tooLong`). */
  next(): Uint8Array | null {
    if (this.tooLong) return null;
    const nl = this.input.indexOf(10, this.at);
    const end = nl < 0 ? this.input.length : nl + 1;
    if (end - this.at > this.max) {
      this.tooLong = true;
      return null;
    }
    if (nl < 0) return null;
    const cr = nl > this.at && this.input[nl - 1] === 13 ? 1 : 0;
    const line = this.input.subarray(this.at, nl - cr);
    this.at = nl + 1;
    return line;
  }

  /** The next whole line as UTF-8 text (see `next()`). */
  nextText(): string | null {
    const l = this.next();
    return l === null ? null : text.decode(l);
  }

  /** Bytes of the whole lines read so far: what to ack. */
  get consumed(): number {
    return this.at;
  }

  /** The input after the lines read (a partial line, or data that follows a command). */
  rest(): Uint8Array {
    return this.input.subarray(this.at);
  }
}
