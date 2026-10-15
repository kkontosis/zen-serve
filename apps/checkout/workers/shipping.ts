// The shipping service: the saga's last step.
import { serveShipping } from '../src/checkout.js';
import { logError, runWorker } from '../src/worker.js';

await runWorker('shipping', (db) => {
  serveShipping(db, { onError: logError });
});
