// The owner of a database among the tabs and processes of one device
// (spec/zendb.md §9.2): whoever holds the Web Lock `zen/db/<fs>/<ns>`.
// Step 6 runs consumers, the scheduler and the stream in the owner; other
// tabs run their own transactions meanwhile (serializable either way). The
// election itself lives in `@zen/client`, shared with `@zen/fs`.
export { type LockManagerLike, Owner } from '@zen/client';
