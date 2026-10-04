// Topics: the encrypted event log, consumer groups and leaders
// (spec/api.md §7–8). (In progress.)
import type { UnlockedFs } from './fs.js';
import type { Path } from './kv.js';

/** A topic of an fs, by path. */
export class Topic {
  constructor(
    readonly fs: UnlockedFs,
    readonly path: Path,
  ) {}
}
