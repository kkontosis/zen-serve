// The plaintext of a connection's sealed state (svc.md §7, formats.md §4
// kind 9): `{v: 1, engine, engine_state, app}`.
import { cbor } from '@zen/db';

/** A decoded state. */
export interface EngineState {
  engine: string;
  engineState: Uint8Array;
  /** The application's state; null when it has none. */
  app: Uint8Array | null;
}

/** Encode a state; an absent `app` is stored as an empty byte string. */
export function encodeState(s: EngineState): Uint8Array {
  return cbor.encode({
    v: 1,
    engine: s.engine,
    engine_state: s.engineState,
    app: s.app ?? new Uint8Array(0),
  });
}

/** Decode a state; throws `format` on anything but version 1. */
export function decodeState(bytes: Uint8Array): EngineState {
  const m = cbor.decode(bytes) as Record<string, unknown> | null;
  if (
    !m ||
    typeof m !== 'object' ||
    m.v !== 1 ||
    typeof m.engine !== 'string' ||
    !(m.engine_state instanceof Uint8Array) ||
    !(m.app instanceof Uint8Array)
  ) {
    throw new Error('format: not a version-1 svc state');
  }
  return {
    engine: m.engine,
    engineState: m.engine_state,
    app: m.app.length ? m.app : null,
  };
}
