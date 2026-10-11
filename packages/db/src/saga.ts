// Sagas (spec/zendb.md §12.5): an orchestrator in the app drives a series
// of commands to participants, each with an optional compensation. Every
// transition is one commit: the `$sagas` row, the next message, the timeout
// and the consume step.
import { bytes, type Topic } from '@zen/client';
import { appendMsg, type ConsumerHandle, checkType, type Delivered } from './broker.js';
import { isPlainObject, type Value } from './cbor.js';
import type { Db } from './db.js';
import { DbError } from './errors.js';
import type { Msg } from './msg.js';
import { dueAt, scheduleMsg } from './sched.js';
import type { DbTransaction } from './txn.js';

/** One step of a saga. */
export interface SagaStep {
  /** The step's name: commands have type `<saga>.<step>`. */
  name: string;
  /** Where its command goes; a participant replies with `serve`. */
  command: Topic;
  /** Where its compensation goes (type `<saga>.<step>.compensate`), if it has one. */
  compensate?: Topic;
  /** How long a reply may take (default 60,000 ms): then the saga compensates. */
  timeoutMs?: number;
}

/** Options of `db.saga`. */
export interface SagaOptions {
  /** The orchestrator's group name (default `"orchestrator"`). */
  group?: string;
  /** The orchestrator's consumer lease (default 10,000 ms). */
  ttlMs?: number;
  /** Errors of the orchestrator's consumers. */
  onError?: (e: unknown) => void;
}

/** Where a saga is. */
export type SagaStatus = 'running' | 'compensating' | 'done' | 'failed' | 'stuck';

/** A saga instance: its `$sagas` row (§11.5). */
export interface SagaState<D extends Record<string, Value> = Record<string, Value>> {
  [field: string]: Value | undefined;
  id: Uint8Array;
  /** The saga's name. */
  saga: string;
  /** The step in flight: its command, or (compensating) its compensation. */
  step: number;
  status: SagaStatus;
  /** The app's data: the start data, with each step's reply body merged in, and `error` on a failure. */
  data: D;
  /** The completed steps, in order. */
  done: number[];
  /** The pending timeout's `$sched` id. */
  timeout?: Uint8Array;
  /** The corr of the message in flight: replies with another are stale. */
  corr?: Uint8Array;
}

/** The message type of a step's timeout. */
export const SAGA_TIMEOUT = 'zen.saga.timeout';

/** Default reply timeout of a step. */
const STEP_TIMEOUT = 60_000;

type Kind = 'ok' | 'error' | 'timeout';

/**
 * A saga definition with its orchestrator (§12.5). The orchestrator runs in
 * the owner (§9.2), from `db.saga(…)` until `stop()` or `db.close()`.
 */
export class Saga<D extends Record<string, Value> = Record<string, Value>> {
  /** The orchestrator's topic: replies and timeouts, keyed by saga id. */
  readonly topic: Topic;
  private readonly handles: ConsumerHandle[] = [];

  /** @internal */
  constructor(
    private readonly db: Db,
    readonly name: string,
    private readonly steps: SagaStep[],
    opts: SagaOptions = {},
  ) {
    if (!steps.length) throw new DbError('bad_value', `saga ${name} has no steps`);
    const names = new Set<string>();
    for (const s of steps) {
      if (!s.name || names.has(s.name)) {
        throw new DbError('bad_value', `saga ${name}: step names must be distinct and non-empty`);
      }
      names.add(s.name);
      checkType(`${name}.${s.name}`);
    }
    this.topic = db.topic('zen', 'saga', db.ns, name);
    const group = opts.group ?? 'orchestrator';
    const common = {
      ...(opts.ttlMs !== undefined ? { ttlMs: opts.ttlMs } : {}),
      ...(opts.onError ? { onError: opts.onError } : {}),
    };
    this.handles.push(
      db.consume({ ...common, group, topic: this.topic, mode: 'per_key' }, (m, tx) =>
        this.handle(m, tx),
      ),
    );
    // A compensation dead-lettered by a `retry: 'delay'` participant leaves
    // the saga stuck (§12.5).
    const dlqs = new Map<string, Topic>();
    for (const s of steps) if (s.compensate) dlqs.set(bytes.hex(s.compensate.id), s.compensate);
    for (const t of dlqs.values()) {
      this.handles.push(
        db.consume(
          {
            ...common,
            group: `${group}.${name}.dlq`,
            topic: t.child('zen.dlq'),
            mode: 'sequential',
          },
          (m, tx) => this.deadLetter(m, tx),
        ),
      );
    }
  }

  /**
   * Start an instance: insert its row, send step 0's command and schedule
   * its timeout, in `tx` (the transaction that created the business
   * object) or a transaction of its own. Returns the saga id.
   */
  async start(data: D, tx?: DbTransaction): Promise<Uint8Array> {
    if (!isPlainObject(data)) throw new DbError('bad_value', 'saga data must be a map');
    const id = bytes.randomBytes(16);
    if (tx) await this.begin(tx, id, data);
    else await this.db.transaction((t) => this.begin(t, id, data));
    return id;
  }

  private async begin(tx: DbTransaction, id: Uint8Array, data: D): Promise<void> {
    const row: SagaState = { id, saga: this.name, step: 0, status: 'running', data, done: [] };
    await this.send(tx, row, 0, false);
    await tx.table<SagaState>('$sagas').insert(row);
  }

  /** An instance's state, or undefined. */
  get(id: Uint8Array): Promise<SagaState<D> | undefined> {
    return this.db.table<SagaState<D>>('$sagas').get(id);
  }

  /** Stop the orchestrator's consumers. */
  async stop(): Promise<void> {
    await Promise.all(this.handles.map((h) => h.stop()));
  }

  /**
   * Send step `i`'s command (or compensation) and schedule its timeout;
   * the row gets the step, corr and timeout id.
   */
  private async send(tx: DbTransaction, row: SagaState, i: number, comp: boolean): Promise<void> {
    const s = this.steps[i]!;
    const corr = bytes.randomBytes(16);
    const type = `${this.name}.${s.name}${comp ? '.compensate' : ''}`;
    const m: Msg = {
      type,
      id: bytes.randomBytes(16),
      corr,
      replyTo: this.topic.path.map((p) => (typeof p === 'string' ? bytes.utf8(p) : p)),
      saga: row.id,
      step: s.name,
      body: row.data,
    };
    appendMsg(tx, comp ? s.compensate! : s.command, m, row.id);
    const t: Msg = {
      type: SAGA_TIMEOUT,
      id: bytes.randomBytes(16),
      corr,
      saga: row.id,
      step: s.name,
    };
    await scheduleMsg(
      tx,
      this.topic,
      t,
      row.id,
      dueAt(this.db, { delayMs: s.timeoutMs ?? STEP_TIMEOUT }),
    );
    row.step = i;
    row.corr = corr;
    row.timeout = t.id;
  }

  /** The newest completed step before `before` that has a compensation, or -1. */
  private nextCompensation(row: SagaState, before: number): number {
    for (let k = row.done.length - 1; k >= 0; k--) {
      const i = row.done[k]!;
      if (i < before && this.steps[i]!.compensate) return i;
    }
    return -1;
  }

  /** Compensate from before `before`, or end `failed`. */
  private async compensate(tx: DbTransaction, row: SagaState, before: number): Promise<void> {
    const i = this.nextCompensation(row, before);
    if (i < 0) {
      row.status = 'failed';
      delete row.corr;
      delete row.timeout;
      return;
    }
    row.status = 'compensating';
    await this.send(tx, row, i, true);
  }

  /** One reply or timeout: one transition (§12.5). */
  private async handle(m: Delivered, tx: DbTransaction): Promise<void> {
    if (!m.saga || !m.corr) return;
    const rows = tx.table<SagaState>('$sagas');
    const row = await rows.get(m.saga);
    // Stale: another saga's, an earlier message's, or a finished one's.
    if (!row || row.saga !== this.name || !row.corr || !bytes.equal(row.corr, m.corr)) return;
    const kind: Kind | undefined =
      m.type === SAGA_TIMEOUT
        ? 'timeout'
        : m.type.endsWith('.error')
          ? 'error'
          : m.type.endsWith('.reply')
            ? 'ok'
            : undefined;
    if (!kind) return;
    // The timeout was delivered; a reply cancels it.
    if (kind !== 'timeout' && row.timeout) await tx.cancel(row.timeout);
    delete row.timeout;
    if (row.status === 'running') {
      if (kind === 'ok') {
        if (isPlainObject(m.body)) row.data = { ...row.data, ...(m.body as Record<string, Value>) };
        row.done.push(row.step);
        if (row.step + 1 < this.steps.length) {
          await this.send(tx, row, row.step + 1, false);
        } else {
          row.status = 'done';
          delete row.corr;
        }
      } else {
        if (kind === 'error') row.data = { ...row.data, error: m.body };
        else
          row.data = {
            ...row.data,
            error: { message: 'timeout', step: this.steps[row.step]!.name },
          };
        await this.compensate(tx, row, row.step);
      }
    } else if (row.status === 'compensating') {
      if (kind === 'ok') {
        await this.compensate(tx, row, row.step);
      } else {
        row.status = 'stuck';
        delete row.corr;
      }
    } else {
      return;
    }
    await rows.put(row);
  }

  /** A dead-lettered compensation of this saga, still in flight: `stuck`. */
  private async deadLetter(m: Delivered, tx: DbTransaction): Promise<void> {
    if (!m.saga || !m.corr) return;
    const rows = tx.table<SagaState>('$sagas');
    const row = await rows.get(m.saga);
    if (!row || row.saga !== this.name || row.status !== 'compensating') return;
    if (!row.corr || !bytes.equal(row.corr, m.corr)) return;
    if (row.timeout) await tx.cancel(row.timeout);
    row.status = 'stuck';
    delete row.timeout;
    delete row.corr;
    await rows.put(row);
  }
}
