// The CRDT filesystem (spec/fs.md, api.md §12). (In progress.)
import type { UnlockedFs } from './fs.js';

/** A tree of an fs. */
export class Tree {
  constructor(
    readonly fs: UnlockedFs,
    readonly id: Uint8Array,
  ) {}
}
