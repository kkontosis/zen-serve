// A Node worker of the kanban app that stands in for a tab killed
// mid-upload: it attaches an endless file to a card, prints the chunk ids
// of each upload commit, and after the first one stalls until it is killed
// (test/kanban.test.ts). Signs in from the environment (harness.workerEnv);
// KANBAN_BOARD and KANBAN_CARD name the card.
import { bytes } from '@zen/client';
import { workerFromEnv } from '../../shared/node.js';
import { attachFile, boardTree, DB_OPTIONS, getBoard, NS } from '../src/kanban.js';

const w = await workerFromEnv();
const db = await w.open(NS, DB_OPTIONS);
const tree = boardTree(w.fs, await getBoard(db, process.env.KANBAN_BOARD!));

let stalled = false;
async function* endless(): AsyncGenerator<Uint8Array> {
  for (;;) {
    if (stalled) await new Promise(() => {});
    yield crypto.getRandomValues(new Uint8Array(65536));
  }
}

// Keep the process up while the source is stalled.
setInterval(() => {}, 1000);
console.log('uploading');
await attachFile(db, tree, process.env.KANBAN_CARD!, 'endless.bin', endless(), {
  upload: {
    onProgress: (s) => {
      console.log(`uploaded ${s.chunks.map((c) => bytes.hex(c)).join(',')}`);
      stalled = true;
    },
  },
});
console.log('attached (never)');
