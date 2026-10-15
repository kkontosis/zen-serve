// The booking app's grants (EXAMPLES.md §3): every member gets these and
// nothing more, in the vitest scenarios and the Playwright specs. The
// scheduler's lease (`("zen", "db", "booking", "sched")`) and the consumer
// groups' leases and claims are under `("zen", "db", "booking")`.
import { FS_RW, type GrantSpec, topicGrant } from '../../shared/harness.js';

export const GRANTS: GrantSpec[] = [
  FS_RW,
  topicGrant(['booking'], ['read', 'append', 'consume']),
  topicGrant(['zen', 'db', 'booking'], ['read', 'append', 'consume']),
];
