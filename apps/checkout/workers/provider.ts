// A mock payment provider over HTTP (node:http), for the effects worker:
// * POST /charge {key, order, customer, amount}: one charge per idempotency
//   key; a repeated key returns the first result. A customer whose name
//   starts with "decline" is declined.
// * POST /refund {key, charge}: refund a charge once per key. A refund of a
//   charge not made yet voids its key: a later charge with it is refused.
// * GET /charges: {charges, refunds}, with the calls each key got.
// Prints `provider listening <url>`. $PROVIDER_PORT picks the port (default any).
import { createServer, type IncomingMessage } from 'node:http';

/** A charge, by idempotency key. */
export interface Charge {
  key: string;
  order: string;
  customer: string;
  amount: number;
  status: 'charged' | 'declined' | 'voided';
  ref: string;
  refunded: boolean;
  /** The calls with this key. */
  calls: number;
}

/** A refund, by idempotency key. */
export interface Refund {
  key: string;
  charge: string;
  status: 'refunded' | 'voided' | 'nothing';
  calls: number;
}

const charges = new Map<string, Charge>();
const refunds = new Map<string, Refund>();
let refs = 0;

async function json(req: IncomingMessage) {
  let s = '';
  for await (const c of req) s += c;
  return JSON.parse(s || '{}');
}

function charge(b: { key: string; order: string; customer: string; amount: number }) {
  let c = charges.get(b.key);
  if (c) c.calls++;
  else {
    c = {
      key: b.key,
      order: b.order,
      customer: b.customer,
      amount: b.amount,
      status: String(b.customer).startsWith('decline') ? 'declined' : 'charged',
      ref: `ch_${++refs}`,
      refunded: false,
      calls: 1,
    };
    charges.set(b.key, c);
  }
  return { status: c.status, ref: c.ref };
}

function refund(b: { key: string; charge: string }) {
  let r = refunds.get(b.key);
  if (r) r.calls++;
  else {
    const c = charges.get(b.charge);
    let status: Refund['status'] = 'nothing';
    if (!c) {
      // Never charge this key from now on.
      charges.set(b.charge, {
        key: b.charge,
        order: '',
        customer: '',
        amount: 0,
        status: 'voided',
        ref: '',
        refunded: false,
        calls: 0,
      });
      status = 'voided';
    } else if (c.status === 'charged' && !c.refunded) {
      c.refunded = true;
      status = 'refunded';
    }
    r = { key: b.key, charge: b.charge, status, calls: 1 };
    refunds.set(b.key, r);
  }
  return { status: r.status };
}

const server = createServer(async (req, res) => {
  const send = (code: number, v: unknown) => {
    res.writeHead(code, { 'content-type': 'application/json' });
    res.end(JSON.stringify(v));
  };
  try {
    if (req.method === 'GET' && req.url === '/charges') {
      return send(200, { charges: [...charges.values()], refunds: [...refunds.values()] });
    }
    if (req.method === 'POST' && req.url === '/charge') return send(200, charge(await json(req)));
    if (req.method === 'POST' && req.url === '/refund') return send(200, refund(await json(req)));
    send(404, { error: 'not found' });
  } catch (e) {
    send(400, { error: String(e) });
  }
});

server.listen(Number(process.env.PROVIDER_PORT ?? 0), '127.0.0.1', () => {
  const a = server.address();
  if (a && typeof a === 'object') console.log(`provider listening http://127.0.0.1:${a.port}`);
});
