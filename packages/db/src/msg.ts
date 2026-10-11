// Broker messages (spec/zendb.md §10): the Msg an event's payload holds,
// large bodies by reference, and event ids for causation.
import { bytes, type Topic } from '@zen/client';
import { decode, encode, isPlainObject, type Value } from './cbor.js';
import { DbError } from './errors.js';

/** A large body stored in parts of the sender's database (§10.3). */
export interface BodyRef {
  ns: string;
  parts: number;
  /** `H("zen/v1/db-parts-digest", CBOR(body))`. */
  digest: Uint8Array;
}

/** A message as encoded in an event (§10.1). */
export interface Msg {
  type: string;
  id: Uint8Array;
  corr?: Uint8Array;
  replyTo?: Uint8Array[];
  deliverAt?: number;
  saga?: Uint8Array;
  step?: string;
  headers?: Record<string, string>;
  body?: Value;
  bodyRef?: BodyRef;
}

/** A received message: the Msg, its body resolved, and where it came from. */
export interface Message<B = Value> {
  type: string;
  id: Uint8Array;
  body: B;
  corr?: Uint8Array;
  replyTo?: Uint8Array[];
  deliverAt?: number;
  saga?: Uint8Array;
  step?: string;
  headers: Record<string, string>;
  /** The topic it was appended to. */
  topic: Topic;
  offset: Uint8Array;
  keyToken?: Uint8Array;
  /** The sending device, as sealed (any member can seal any value, §14). */
  sender: Uint8Array;
  hlc: bigint;
  /** The causing event's id (empty for none, §10.2). */
  causation: Uint8Array;
  /** This event's id: topic id ‖ offset (§10.2). */
  eventId: Uint8Array;
}

/** An event's id: `topic_id ‖ offset` (§10.2). */
export const eventId = (topic: Uint8Array, offset: Uint8Array): Uint8Array =>
  bytes.concat(topic, offset);

export function encodeBodyRef(r: BodyRef): Uint8Array {
  return encode(bodyRefValue(r));
}

const bodyRefValue = (r: BodyRef) =>
  new Map<Value, Value>([
    [1, r.ns],
    [2, r.parts],
    [3, r.digest],
  ]);

function decodeBodyRef(v: Value): BodyRef {
  const ns = v instanceof Map ? v.get(1) : undefined;
  const parts = v instanceof Map ? v.get(2) : undefined;
  const digest = v instanceof Map ? v.get(3) : undefined;
  if (
    typeof ns !== 'string' ||
    typeof parts !== 'number' ||
    !Number.isInteger(parts) ||
    parts < 1 ||
    !(digest instanceof Uint8Array) ||
    digest.length !== 32
  ) {
    throw new DbError('poisoned', 'a malformed BodyRef');
  }
  return { ns, parts, digest };
}

export function encodeMsg(m: Msg): Uint8Array {
  const v = new Map<Value, Value>([
    [1, m.type],
    [2, m.id],
  ]);
  if (m.corr) v.set(3, m.corr);
  if (m.replyTo) v.set(4, m.replyTo);
  if (m.deliverAt !== undefined) v.set(5, m.deliverAt);
  if (m.saga) v.set(6, m.saga);
  if (m.step !== undefined) v.set(7, m.step);
  if (m.headers && Object.keys(m.headers).length) v.set(8, m.headers);
  if (m.body !== undefined) v.set(9, m.body);
  if (m.bodyRef) v.set(10, bodyRefValue(m.bodyRef));
  return encode(v);
}

const isId = (x: unknown): x is Uint8Array => x instanceof Uint8Array && x.length === 16;

/** Decode a Msg; anything else is `poisoned`. */
export function decodeMsg(b: Uint8Array): Msg {
  let v: Value;
  try {
    v = decode(b);
  } catch (e) {
    throw new DbError('poisoned', `not a message: ${e instanceof Error ? e.message : e}`);
  }
  const bad = (what: string) => new DbError('poisoned', `a message with a bad ${what}`);
  if (!(v instanceof Map)) throw bad('encoding');
  const type = v.get(1);
  const id = v.get(2);
  if (typeof type !== 'string') throw bad('type');
  if (!isId(id)) throw bad('id');
  const m: Msg = { type, id };
  const corr = v.get(3);
  if (corr !== undefined) {
    if (!isId(corr)) throw bad('corr');
    m.corr = corr;
  }
  const replyTo = v.get(4);
  if (replyTo !== undefined) {
    if (!Array.isArray(replyTo) || !replyTo.every((s) => s instanceof Uint8Array)) {
      throw bad('reply_to');
    }
    m.replyTo = replyTo as Uint8Array[];
  }
  const at = v.get(5);
  if (at !== undefined) {
    if (typeof at !== 'number' || !Number.isInteger(at) || at < 0) throw bad('deliver_at');
    m.deliverAt = at;
  }
  const saga = v.get(6);
  if (saga !== undefined) {
    if (!isId(saga)) throw bad('saga');
    m.saga = saga;
  }
  const step = v.get(7);
  if (step !== undefined) {
    if (typeof step !== 'string') throw bad('step');
    m.step = step;
  }
  const headers = v.get(8);
  if (headers !== undefined) {
    if (!isPlainObject(headers) || !Object.values(headers).every((h) => typeof h === 'string')) {
      throw bad('headers');
    }
    m.headers = headers as Record<string, string>;
  }
  if (v.has(9)) m.body = v.get(9);
  if (v.has(10)) {
    if (v.has(9)) throw bad('body (both body and body_ref)');
    m.bodyRef = decodeBodyRef(v.get(10)!);
  }
  return m;
}
