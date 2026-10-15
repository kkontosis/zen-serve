// What the checkout workers share (Node only): sign in from the
// environment (harness.workerEnv), open `shop`, and the settings tests pass
// through the environment:
// * CHECKOUT_TIMEOUTS: JSON `Partial<Timeouts>` (the orchestrator's)
// * CHECKOUT_SCHEDULER: JSON `SchedulerOptions` (every worker runs a scheduler)
// * PROVIDER_URL: the payment provider (the effects worker)
import type { Db, SchedulerOptions } from '@zen/db';
import { workerFromEnv } from '../../shared/node.js';
import { DB_OPTIONS, NS, type Timeouts } from './checkout.js';

const json = <T>(v: string | undefined): T | undefined => (v ? (JSON.parse(v) as T) : undefined);

/** Sign in, open `shop`, and close on SIGTERM. Prints `<name> ready` once `start` ran. */
export async function runWorker(name: string, start: (db: Db) => void | Promise<void>) {
  const w = await workerFromEnv();
  const sched = json<SchedulerOptions>(process.env.CHECKOUT_SCHEDULER);
  const db = await w.open(NS, {
    ...DB_OPTIONS,
    ...(sched ? { schedulerOptions: sched } : {}),
    warn: (m) => console.log(`warn: ${m}`),
  });
  await start(db);
  console.log(`${name} ready`);
  process.on('SIGTERM', async () => {
    await w.close();
    process.exit(0);
  });
}

/** The timeouts from the environment. */
export const envTimeouts = (): Partial<Timeouts> =>
  json<Partial<Timeouts>>(process.env.CHECKOUT_TIMEOUTS) ?? {};

/** Errors of the consumers, printed. */
export const logError = (e: unknown) =>
  console.log(`error: ${e instanceof Error ? e.message : String(e)}`);
