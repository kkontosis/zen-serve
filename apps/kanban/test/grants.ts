// The kanban app's grants (docs/EXAMPLES.md §1 "Grants"): every member gets
// these and nothing more, in the vitest scenarios and the Playwright specs.
import { FS_RW, type GrantSpec, topicGrant } from '../../shared/harness.js';

export const GRANTS: GrantSpec[] = [
  // The boards' trees: card directories and attachments.
  FS_RW,
  // ("board", id): the ephemeral "dragging…" cursors.
  topicGrant(['board'], ['read', 'append']),
  // The database's own topics: the change topics of `cards` and
  // `crdt_cards`, the scheduler's lease, and the indexer of the CRDT cards'
  // (column, pos) index, which is a consumer group.
  topicGrant(['zen', 'db', 'kanban'], ['read', 'append', 'consume']),
  // The cards' long texts (Loro documents).
  topicGrant(['zen', 'loro', 'kanban'], ['read', 'append']),
];
