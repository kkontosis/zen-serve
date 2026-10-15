// A Node worker of the hello app: signs in from the environment
// (harness.workerEnv), follows the notes topic and prints each note.
// Built to apps/dist/hello/workers/listener.mjs by `npm run build:apps`.
import { workerFromEnv } from '../../shared/node.js';
import { DB_OPTIONS, NS, onNotes } from '../src/hello.js';

const w = await workerFromEnv();
const db = await w.open(NS, DB_OPTIONS);
await onNotes(db, (m) => console.log(`note: ${m.body.text}`));
console.log('listening');
process.on('SIGTERM', async () => {
  await w.close();
  process.exit(0);
});
