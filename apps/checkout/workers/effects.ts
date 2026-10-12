// The effects worker: charges and refunds at the payment provider
// ($PROVIDER_URL), with the command's id as the idempotency key, and
// replies to the saga in the transaction that consumes the command.
// $CHECKOUT_HANG_AFTER_CHARGE=1 (tests): print `charged <key>` after the
// provider call and hang there, so the test can kill the worker before
// it commits.
import { httpProvider, serveEffects } from '../src/checkout.js';
import { logError, runWorker } from '../src/worker.js';

const url = process.env.PROVIDER_URL;
if (!url) throw new Error('$PROVIDER_URL is not set');
const hang = process.env.CHECKOUT_HANG_AFTER_CHARGE === '1';

await runWorker('effects', (db) => {
  serveEffects(db, httpProvider(url), {
    onError: logError,
    ...(hang
      ? {
          afterCall: async (key: string) => {
            console.log(`charged ${key}`);
            await new Promise(() => {});
          },
        }
      : {}),
  });
});
