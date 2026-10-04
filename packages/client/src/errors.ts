// Errors.

/** An error response of the server (spec/api.md §1), or a transport failure (`status` 0). */
export class ZenError extends Error {
  override name = 'ZenError';
  constructor(
    /** HTTP status; 0 when the request never got a response. */
    readonly status: number,
    /** The error code: `conflict`, `forbidden`, `stale_op`, …; `network` for transport failures. */
    readonly code: string,
    message: string,
    /** `Retry-After`, in ms, when the server sent one. */
    readonly retryAfterMs?: number,
  ) {
    super(message ? `${code}: ${message}` : code);
  }

  /** Whether the whole transaction may be retried (`conflict`, `too_old`). */
  get retryable(): boolean {
    return this.status === 409 && (this.code === 'conflict' || this.code === 'too_old');
  }
}

/** Whether `e` is a ZenError with this code. */
export function isCode(e: unknown, code: string): e is ZenError {
  return e instanceof ZenError && e.code === code;
}
