// The ledger app's grants (EXAMPLES.md §5): every member gets these and
// nothing more, in the vitest scenarios and the Playwright specs.
import { FS_RW, type GrantSpec, topicGrant } from '../../shared/harness.js';

export const GRANTS: GrantSpec[] = [
  FS_RW,
  topicGrant(['ledger'], ['read', 'append', 'consume']),
  topicGrant(['zen', 'db', 'ledger'], ['read', 'append', 'consume']),
];
