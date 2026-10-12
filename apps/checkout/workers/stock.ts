// The stock service: reserves and releases stock for the checkout saga.
// Built to apps/dist/checkout/workers/stock.mjs by `npm run build:apps`.
import { serveStock } from '../src/checkout.js';
import { logError, runWorker } from '../src/worker.js';

await runWorker('stock', (db) => {
  serveStock(db, { onError: logError });
});
