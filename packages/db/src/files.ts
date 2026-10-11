// File operations in a zen-db transaction (spec/zendb.md §7.6): a tree's
// operations ride in the same commit as the rows. Chunks are uploaded
// before (`Tree.upload`), so a write costs only its operation here.
import { bytes, type PendingOp, type Tree, TreeBatch, type Upload } from '@zen/client';
import { DbError } from './errors.js';
import type { DbTransaction } from './txn.js';

/** One tree's operations in a transaction: `tx.fs(tree)`. */
export class TxFiles {
  constructor(
    readonly tx: DbTransaction,
    readonly tree: Tree,
  ) {}

  /**
   * Write an uploaded file to its node. `replaces`: the versions it
   * replaces (default `'current'`: the node's versions, read now).
   */
  async write(upload: Upload, opts: { replaces?: Uint8Array[] | 'current' } = {}): Promise<void> {
    this.tx.live();
    await this.ops([await this.tree.writeOp(upload, opts.replaces)]);
  }

  /**
   * Add operations: a `TreeBatch` (mkdir, create, move, rename, remove)
   * or `PendingOp`s. On `stale_op` or `clock_skew` they are rebased and
   * the transaction runs again (fs.md §3.4).
   */
  async ops(batch: TreeBatch | PendingOp[]): Promise<void> {
    this.tx.live();
    const ops = batch instanceof TreeBatch ? batch.ops : batch;
    const mine = new Set<string>();
    for (const o of ops) {
      if (o.op !== 'write') continue;
      const k = `${bytes.hex(this.tree.id)}/${bytes.hex(o.node)}`;
      if (this.tx.fileWrites.has(k) || mine.has(k)) {
        throw new DbError('bad_value', 'one write per file per commit (fs.md)');
      }
      mine.add(k);
    }
    for (const k of mine) this.tx.fileWrites.add(k);
    await this.tree.writeIn(this.tx.raw, ops);
  }
}
