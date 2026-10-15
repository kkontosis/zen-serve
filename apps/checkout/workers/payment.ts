// The payment service: turns `pay` and `refund` into commands on the
// effects topics (zendb.md §12.6); the effects worker replies to the saga.
import { servePayment } from '../src/checkout.js';
import { logError, runWorker } from '../src/worker.js';

await runWorker('payment', (db) => {
  servePayment(db, { onError: logError });
});
