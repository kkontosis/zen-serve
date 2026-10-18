// The echo app's grants: fs read/write for the `lines` table, and topic
// `consume` on the relayed services (`svc/…`), which serving needs.
import { FS_RW, type GrantSpec, topicGrant } from '../../shared/harness.js';

export const GRANTS: GrantSpec[] = [
  FS_RW,
  topicGrant(['svc'], ['consume']),
  topicGrant(['zen', 'db', 'echo'], ['read', 'append', 'consume']),
];
