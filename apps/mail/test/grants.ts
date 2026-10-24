// The mail app's grants: fs read/write for its tables, topic `consume` on
// the relayed services (`svc/…`), which serving needs, and the app's own
// topics (`mail/received`).
import { FS_RW, type GrantSpec, topicGrant } from '../../shared/harness.js';

export const GRANTS: GrantSpec[] = [
  FS_RW,
  topicGrant(['svc'], ['consume']),
  topicGrant(['mail'], ['read', 'append', 'consume']),
  topicGrant(['zen', 'db', 'mail'], ['read', 'append', 'consume']),
];
