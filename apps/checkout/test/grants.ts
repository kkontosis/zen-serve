// The checkout app's grants (EXAMPLES.md §4): every member, the page's
// user and each service's worker, gets these and nothing more.
// * ("shop"): the services' command topics, the effects topics and the
//   compensations' class DLQs (`<topic>/zen.dlq`, which the orchestrator
//   consumes)
// * ("zen", "saga", "shop"): the saga's replies and timeouts
// * ("zen", "db", "shop"): the scheduler's lease and the orders' change events
// * ("zen", "inbox"): request/reply inboxes (EXAMPLES.md lists it; the
//   saga's messages don't use them)
import { FS_RW, type GrantSpec, topicGrant } from '../../shared/harness.js';

export const GRANTS: GrantSpec[] = [
  FS_RW,
  topicGrant(['shop'], ['read', 'append', 'consume']),
  topicGrant(['zen', 'saga', 'shop'], ['read', 'append', 'consume']),
  topicGrant(['zen', 'db', 'shop'], ['read', 'append', 'consume']),
  topicGrant(['zen', 'inbox'], ['read', 'append', 'consume']),
];
