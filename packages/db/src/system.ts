// The system tables every database has (spec/zendb.md §11.5). They are
// created with the DbRecord (or on the first open of an older database),
// and app tables may not take names starting with `$`.
import { bytes } from '@zen/client';
import type { IndexDef, TableRecord } from './catalog.js';
import { DbError } from './errors.js';

const DEFS: { name: string; pk: string[]; index?: Omit<IndexDef, 'id'> }[] = [
  { name: '$cursors', pk: ['device', 'name'] },
  {
    name: '$sched',
    pk: ['id'],
    index: {
      name: 'by_at',
      fields: [['at', 'int', false]],
      kind: 'private',
      unique: false,
      state: 'active',
    },
  },
  { name: '$sagas', pk: ['id'] },
  { name: '$inbox', pk: ['scope', 'id'] },
  { name: '$ixrows', pk: ['table_id', 'pk'] },
];

/** The names of the system tables. */
export const SYSTEM_TABLES = DEFS.map((d) => d.name);

/** A new system table's record (empty, so its index is active at once). */
export function systemTable(name: string): TableRecord {
  const d = DEFS.find((x) => x.name === name)!;
  return {
    name,
    id: bytes.randomBytes(16),
    pk: d.pk,
    indexes: d.index ? [{ ...d.index, id: bytes.randomBytes(16) }] : [],
    pad: false,
    state: 'active',
  };
}

/** Refuse an app table name in the system namespace (`bad_value`). */
export function checkAppTable(name: string): void {
  if (name.startsWith('$')) {
    throw new DbError('bad_value', `table names starting with $ are reserved (${name})`);
  }
}
