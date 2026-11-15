// `zen-git setup` (spec/git.md §10): the topic grants of `("git")`, once per fs, by an admin.
import { bytes, type Session } from '@zen/client';
import type { SigningIdentity } from '@zen/wasm';

/** What `setupGrants` added. */
export interface SetupResult {
  added: number;
}

/**
 * Grant `append` on the topic prefix `("git")` to every member with fs
 * `write`, and `read` and `consume` to every member with fs `read`. Grants
 * already there are kept; only the missing rights are added.
 */
export async function setupGrants(
  session: Session,
  admin: SigningIdentity,
  fs: number,
  topicId: Uint8Array,
): Promise<SetupResult> {
  let added = 0;
  await session.acl.update(admin, (doc) => {
    added = 0;
    const want = new Map<string, { subject: Uint8Array; rights: Set<string> }>();
    for (const g of doc.grants) {
      if (g.fs !== fs || g.topic !== undefined) continue;
      const k = bytes.hex(g.subject);
      const w = want.get(k) ?? { subject: g.subject, rights: new Set<string>() };
      if (g.rights.includes('write')) w.rights.add('append');
      if (g.rights.includes('read')) {
        w.rights.add('read');
        w.rights.add('consume');
      }
      want.set(k, w);
    }
    for (const [k, w] of want) {
      const have = new Set(
        doc.grants
          .filter(
            (g) =>
              g.fs === fs &&
              g.topic !== undefined &&
              bytes.hex(g.subject) === k &&
              bytes.equal(g.topic, topicId),
          )
          .flatMap((g) => g.rights),
      );
      const rights = [...w.rights].filter((r) => !have.has(r)).sort();
      if (!rights.length) continue;
      doc.grants.push({ fs, topic: topicId, rights, subject: w.subject });
      added++;
    }
  });
  return { added };
}
