// The hello app's grants (EXAMPLES.md lists each app's): every member gets
// these and nothing more, in the vitest scenarios and the Playwright specs.
import { FS_RW, type GrantSpec, topicGrant } from '../../shared/harness.js';

export const GRANTS: GrantSpec[] = [
  FS_RW,
  topicGrant(['hello'], ['read', 'append']),
  topicGrant(['zen', 'loro', 'hello'], ['read', 'append']),
  topicGrant(['zen', 'db', 'hello'], ['read', 'append', 'consume']),
];
