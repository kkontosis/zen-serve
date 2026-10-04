// The WebSocket stream (spec/api.md §9). (In progress.)
import type { Session } from './session.js';

/** An open stream. */
export class Stream {
  private constructor(readonly session: Session) {}

  /** Open and authenticate a stream. */
  static async open(session: Session): Promise<Stream> {
    return new Stream(session);
  }
}
