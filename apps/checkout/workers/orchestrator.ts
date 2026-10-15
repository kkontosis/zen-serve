// The checkout saga's orchestrator, and the group that keeps the orders'
// status in step with the sagas. Restartable: each transition is one
// commit, and a killed worker's claims expire. $CHECKOUT_TIMEOUTS must match
// the timeouts of the process that places the orders.
import { checkoutSaga, trackStatus } from '../src/checkout.js';
import { envTimeouts, logError, runWorker } from '../src/worker.js';

await runWorker('orchestrator', (db) => {
  const saga = checkoutSaga(db, { timeouts: envTimeouts(), onError: logError });
  trackStatus(db, saga, { onError: logError });
});
