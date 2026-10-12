// The checkout services as Node workers, for the vitest scenarios and the
// Playwright specs: the mock provider and one worker per service, each
// signed in as its own member (its own device). Imports only the harness,
// so Playwright (no @zen aliases) can load it.
import { spawnWorker, type WorkerProcess, type World, workerEnv } from '../../shared/harness.js';

/** The services, each a worker and a member of the same name. */
export const SERVICES = ['stock', 'payment', 'effects', 'shipping', 'orchestrator'] as const;
export type Service = (typeof SERVICES)[number];

/** The members of a checkout world: the customer's and the services'. */
export const USERS = ['alice', ...SERVICES];

/** Small scheduler settings, so a killed scheduler is replaced fast. */
export const SCHEDULER = { pollMs: 100, ttlMs: 2000 };

/** Start the mock provider; resolves with it and its URL. */
export async function startProvider(): Promise<{ p: WorkerProcess; url: string }> {
  const p = spawnWorker('checkout', 'provider');
  await p.waitFor('provider listening');
  const line = p.output.find((l) => l.includes('provider listening'))!;
  return { p, url: line.split(' ').pop()! };
}

/** The running services. */
export class Services {
  readonly procs = new Map<Service, WorkerProcess>();
  /** Every process started, for the final clean-up. */
  readonly all: WorkerProcess[] = [];

  constructor(
    private readonly w: World,
    private readonly env: Record<string, string>,
  ) {}

  /** Start (or restart) a service, with extra environment. */
  async start(s: Service, extra: Record<string, string> = {}, wait = true): Promise<WorkerProcess> {
    const p = spawnWorker(
      'checkout',
      s,
      workerEnv(this.w, s, {
        CHECKOUT_SCHEDULER: JSON.stringify(SCHEDULER),
        ...this.env,
        ...extra,
      }),
    );
    this.procs.set(s, p);
    this.all.push(p);
    if (wait) await p.waitFor(`${s} ready`, 60_000);
    return p;
  }

  /** Start every service (but `except`). */
  async startAll(except: Service[] = []): Promise<void> {
    await Promise.all(SERVICES.filter((s) => !except.includes(s)).map((s) => this.start(s)));
  }

  /** SIGKILL a service's current process. */
  async kill(s: Service): Promise<void> {
    await this.procs.get(s)?.kill();
    this.procs.delete(s);
  }

  /** Kill every process started. */
  async stop(): Promise<void> {
    await Promise.all(this.all.map((p) => p.kill()));
    this.procs.clear();
  }

  /** The output of every process started, for a failure's report. */
  dump(): string {
    return this.all.map((p, i) => `--- #${i}\n${p.output.join('\n')}`).join('\n');
  }
}

/** The provider's records (workers/provider.ts `GET /charges`). */
export interface Charges {
  charges: {
    key: string;
    order: string;
    amount: number;
    status: string;
    refunded: boolean;
    calls: number;
  }[];
  refunds: { key: string; charge: string; status: string; calls: number }[];
}

export async function providerCharges(url: string): Promise<Charges> {
  return (await (await fetch(`${url}/charges`)).json()) as Charges;
}
