// The test adversary (zen-server `test-utils`, `adversary = true`): make the
// server lie to readers by freezing state "as it is now" (see
// crates/zen-server/src/adversary.rs). Admins only.
import type { Session } from '../session.js';
import { zw } from '../wasm.js';
import { type CborItem, CborMap, cbor } from './cbor.js';

/** A lie: what to freeze. Writes still apply; reads see the frozen state. */
export type Lie =
  /** A KV value (a stored key): its value or absence, and version. */
  | { kind: 'kv'; fs: number; key: Uint8Array }
  /** The fs header. */
  | { kind: 'header'; fs: number }
  /** A topic up to its head: later events are withheld. */
  | { kind: 'topic'; fs: number; topic: Uint8Array }
  /** A tree's header and changes. */
  | { kind: 'tree'; fs: number; tree: Uint8Array }
  /** A device's checkpoints up to its head. */
  | { kind: 'checkpoints'; fs: number; device: Uint8Array };

const map = (o: Record<string, CborItem>) => new CborMap(Object.entries(o));

/** Add lies (after dropping every earlier one, with `clear`). */
export async function lie(
  session: Session,
  lies: Lie[],
  opts: { clear?: boolean } = {},
): Promise<void> {
  const body = map({ lies: lies.map((l) => map(l)), clear: opts.clear ?? false });
  await session.call('/v1/test/adversary', () => cbor(body), zw.decodeEmpty, undefined);
}

/** Drop every lie: the server answers honestly again. */
export function truth(session: Session): Promise<void> {
  return lie(session, [], { clear: true });
}
