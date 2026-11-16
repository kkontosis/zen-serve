// `zen-git setup` (spec/git.md §10): the topic grants of `("git")` and of the
// scheduler's lease topic, once per fs, by an admin.
import { bytes, type Session } from '@zen/client';
import type { SigningIdentity } from '@zen/wasm';

/** What `setupGrants` added. */
export interface SetupResult {
  added: number;
}

/**
 * Grant `append` on the topic prefix `("git")` to every member with fs
 * `write`, and `read` and `consume` to every member with fs `read`; and
 * `read` and `consume` on the scheduler's topic `("zen", "db", "git",
 * "sched")` to every member with fs `write` (§9). Grants already there are
 * kept; only the missing rights are added.
 */
export async function setupGrants(
  session: Session,
  admin: SigningIdentity,
  fs: number,
  topicId: Uint8Array,
  schedId: Uint8Array,
): Promise<SetupResult> {
  let added = 0;
  await session.acl.update(admin, (doc) => {
    added = 0;
    // (topic, subject) → the rights wanted there.
    const want = new Map<string, { topic: Uint8Array; subject: Uint8Array; rights: Set<string> }>();
    const add = (topic: Uint8Array, subject: Uint8Array, ...rights: string[]) => {
      const k = `${bytes.hex(topic)}/${bytes.hex(subject)}`;
      const w = want.get(k) ?? { topic, subject, rights: new Set<string>() };
      for (const r of rights) w.rights.add(r);
      want.set(k, w);
    };
    for (const g of doc.grants) {
      if (g.fs !== fs || g.topic !== undefined) continue;
      if (g.rights.includes('write')) {
        add(topicId, g.subject, 'append');
        add(schedId, g.subject, 'read', 'consume');
      }
      if (g.rights.includes('read')) add(topicId, g.subject, 'read', 'consume');
    }
    for (const w of want.values()) {
      const have = new Set(
        doc.grants
          .filter(
            (g) =>
              g.fs === fs &&
              g.topic !== undefined &&
              bytes.equal(g.subject, w.subject) &&
              bytes.equal(g.topic, w.topic),
          )
          .flatMap((g) => g.rights),
      );
      const rights = [...w.rights].filter((r) => !have.has(r)).sort();
      if (!rights.length) continue;
      doc.grants.push({ fs, topic: w.topic, rights, subject: w.subject });
      added++;
    }
  });
  return { added };
}
