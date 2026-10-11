// A prolly-tree node store in memory, for tests.
import { bytes } from '@zen/client';
import { expect } from 'vitest';
import { decodeNode, decodeRoot, encodeRoot, type Node } from '../src/index/node.js';
import type { NodeStore, Rewritten } from '../src/index/prolly.js';

const hex = bytes.hex;

/** A node store in memory. */
export class MemStore implements NodeStore {
  readonly m = new Map<string, Uint8Array>();
  root: Uint8Array | undefined;

  async get(ids: Uint8Array[]): Promise<Node[]> {
    return ids.map((id) => {
      const b = this.m.get(hex(id));
      if (!b) throw new Error(`missing node ${hex(id)}`);
      return decodeNode(b);
    });
  }

  apply(r: Rewritten): void {
    for (const w of r.put) {
      expect(this.m.has(hex(w.id)), 'a node written twice').toBe(false);
      this.m.set(hex(w.id), w.bytes);
    }
    for (const id of r.del) {
      expect(this.m.delete(hex(id)), 'a deleted node that was not there').toBe(true);
    }
    this.root = r.root && encodeRoot(r.root);
  }

  rootRecord() {
    return this.root && decodeRoot(this.root);
  }
}
