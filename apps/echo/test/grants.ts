// The echo app's grants: fs read/write for the `lines` table, and topic
// `consume` on the relayed services (`svc/…`), which serving needs, and
// `append`, which connecting to a `tcp-auth` service needs.
import { FS_RW, type GrantSpec, topicGrant } from '../../shared/harness.js';

export const GRANTS: GrantSpec[] = [
  FS_RW,
  topicGrant(['svc'], ['consume', 'append']),
  topicGrant(['zen', 'db', 'echo'], ['read', 'append', 'consume']),
];

/** A member who serves but may not connect (no `append` on `svc/`). */
export const HANDLER_ONLY: GrantSpec[] = [
  FS_RW,
  topicGrant(['svc'], ['consume']),
  topicGrant(['zen', 'db', 'echo'], ['read', 'append', 'consume']),
];
