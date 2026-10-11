// zen-db errors. Server errors reach the caller as `ZenError` (@zen/client).

/** What went wrong in zen-db itself (spec/zendb.md). */
export type DbErrorCode =
  /** `insert` of a row that exists (§4.4). */
  | 'exists'
  /** `update` of a missing row, or an unknown table or index. */
  | 'not_found'
  /** A unique index already holds the value for another row (§5.2). */
  | 'unique_violation'
  /** A value of the wrong type for an indexed field (§5.1). */
  | 'bad_type'
  /** A value CBOR can't hold (§1), or a row without its pk. */
  | 'bad_value'
  /** The stored schema is newer than the app's (§3.1). */
  | 'schema_newer'
  /** An `orderBy` no index provides, on a result larger than the limit (§6.1). */
  | 'needs_index'
  /** A commit over the server's limits, or a sort key over 4,096 bytes (§5.1, §7.5). */
  | 'too_large'
  /** A stored value that fails its checks (§4.2). */
  | 'corrupt'
  /** An unknown format, integrity level or index kind (§3). */
  | 'format'
  /** A unique index whose backfill found duplicates (§8.2). */
  | 'building';

/** A zen-db error. */
export class DbError extends Error {
  override name = 'DbError';

  constructor(
    readonly code: DbErrorCode,
    message: string,
    /** Extra detail, e.g. the duplicates of a unique build. */
    readonly detail?: unknown,
  ) {
    super(`${code}: ${message}`);
  }
}
