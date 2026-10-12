// The chat app's grants (EXAMPLES.md §2 "Grants"): every member gets these
// and nothing more, in the vitest scenarios and the Playwright specs.
import { FS_RW, type GrantSpec, topicGrant } from '../../shared/harness.js';

export const GRANTS: GrantSpec[] = [
  // Photos are fs files.
  FS_RW,
  // Messages ("chat", room) and typing indicators ("chat", room, "typing").
  topicGrant(['chat'], ['read', 'append']),
  // Presence requests.
  topicGrant(['presence'], ['read', 'append']),
  // Each Db's reply inbox ("zen", "inbox", instance): its replies are read
  // there, and replies to the others' requests are published there.
  topicGrant(['zen', 'inbox'], ['read', 'append']),
  // The change topics of `rooms` and `reactions` ("zen", "db", "chat",
  // "changes", table). No `consume`: the app runs no consumer, and
  // DB_OPTIONS turns the scheduler and the indexer off.
  topicGrant(['zen', 'db', 'chat'], ['read', 'append']),
];
