// The signed ACL and membership log (spec/formats.md §9, api.md §4.1–4.2).
import type { AclDoc, Grant, Member, SignedAcl, SigningIdentity } from '@zen/wasm';
import { equal } from './bytes.js';
import type { Client } from './client.js';
import { ZenError } from './errors.js';
import type { Session } from './session.js';
import { zw } from './wasm.js';

/** A verified ACL version. */
export interface AclVersion {
  version: bigint;
  doc: AclDoc;
  /** The signed document bytes, as received (hashing and signing use these). */
  docBytes: Uint8Array;
  signed: SignedAcl;
}

/** A member: an identity and its device certificates. */
export function member(identity: SigningIdentity | Uint8Array, certs: Uint8Array[] = []): Member {
  return {
    identity: identity instanceof Uint8Array ? identity : identity.publicIdentity,
    devices: certs,
  };
}

/** Full rights for `subject` on fs `fs`: read/write, and every topic. */
export function fullGrants(fs: number, subject: Uint8Array): Grant[] {
  return [
    { fs, rights: ['read', 'write'], subject },
    { fs, topic: new Uint8Array(0), rights: ['read', 'append', 'consume'], subject },
  ];
}

function sign(doc: AclDoc, signer: SigningIdentity): Uint8Array {
  const docBytes = zw.encodeAclDoc(doc);
  return zw.encodeSignedAcl({
    doc: docBytes,
    sig: signer.sign(zw.label('SIG_ACL'), docBytes),
    signer: signer.fingerprint,
  });
}

/**
 * Claim a fresh server (api.md §4.1): sign ACL version 1 with `admin` as
 * its first admin and member, and send it with the claim token. `doc`
 * adds grants, members and limits; the admin is added if missing.
 */
export async function claim(
  client: Client,
  claimToken: string,
  admin: SigningIdentity,
  doc: Partial<AclDoc> & { members?: Member[] },
): Promise<bigint> {
  const fp = admin.fingerprint;
  const members = doc.members ?? [];
  if (!members.some((m) => equal(zw.fingerprint(m.identity), fp))) members.unshift(member(admin));
  const full: AclDoc = {
    admins: doc.admins ?? [fp],
    grants: doc.grants ?? [],
    limits: doc.limits ?? [],
    members,
    ...(doc.origins?.length ? { origins: doc.origins } : {}),
    version: 1n,
    prev_hash: new Uint8Array(32),
  };
  const r = await client.transport.call('/v1/acl/put', zw.encodeAclPut, zw.decodeAclVersion, {
    acl: sign(full, admin),
    claim: claimToken,
    origin: client.origin,
  });
  return r.version;
}

/** The ACL as seen by a session. */
export class Acl {
  /** The newest version this client verified; later reads must extend it. */
  private pinned: AclVersion | undefined;

  constructor(private readonly session: Session) {}

  /** The signed versions from `from` (default: the head) up to the head. */
  async entries(from?: bigint): Promise<{ head: bigint; entries: Uint8Array[] }> {
    return this.session.call(
      '/v1/acl/get',
      zw.encodeAclGet,
      zw.decodeAclEntries,
      from === undefined ? {} : { from },
    );
  }

  /**
   * Fetch and verify the chain (formats.md §9.3) from the pinned version, or
   * from version 1 the first time, and return the head. Each version must
   * name the previous one's hash and be signed by one of its admins.
   */
  async head(): Promise<AclVersion> {
    const from = this.pinned ? this.pinned.version : 1n;
    const { entries } = await this.entries(from);
    let prev = this.pinned;
    for (const bytes of entries) {
      const v = parse(bytes);
      if (prev && v.version === prev.version) {
        if (!equal(v.docBytes, prev.docBytes)) throw chainError('the pinned version changed');
        continue;
      }
      verifyNext(prev, v);
      prev = v;
    }
    if (!prev) throw chainError('no ACL');
    this.pinned = prev;
    return prev;
  }

  /**
   * Change the ACL: `edit` gets a copy of the head document; the result is
   * signed by `signer` (an admin of the head) as the next version. Retries
   * on a concurrent change (409 `version_mismatch`) up to `attempts` times.
   */
  async update(
    signer: SigningIdentity,
    edit: (doc: AclDoc) => void | Promise<void>,
    attempts = 3,
  ): Promise<AclVersion> {
    for (let i = 1; ; i++) {
      const head = await this.head();
      const doc = zw.decodeAclDoc(head.docBytes);
      await edit(doc);
      doc.version = head.version + 1n;
      doc.prev_hash = zw.aclHash(head.docBytes);
      try {
        await this.session.call('/v1/acl/put', zw.encodeAclPut, zw.decodeAclVersion, {
          acl: sign(doc, signer),
        });
        return this.head();
      } catch (e) {
        if (!(e instanceof ZenError && e.code === 'version_mismatch') || i >= attempts) throw e;
      }
    }
  }
}

function chainError(msg: string): ZenError {
  return new ZenError(0, 'acl_chain', msg);
}

function parse(bytes: Uint8Array): AclVersion {
  const signed = zw.decodeSignedAcl(bytes);
  const doc = zw.decodeAclDoc(signed.doc);
  return { version: doc.version, doc, docBytes: signed.doc, signed };
}

/** Check `next` against `prev` (formats.md §9.3 rules 1–2, and the signature). */
export function verifyNext(prev: AclVersion | undefined, next: AclVersion): void {
  const want = prev ? prev.version + 1n : 1n;
  if (next.version !== want) throw chainError(`version ${next.version}, expected ${want}`);
  const prevHash = prev ? zw.aclHash(prev.docBytes) : new Uint8Array(32);
  if (!equal(next.doc.prev_hash, prevHash)) throw chainError('prev_hash mismatch');
  const authority = prev ? prev.doc : next.doc;
  if (!authority.admins.some((a) => equal(a, next.signed.signer))) {
    throw chainError('signer is not an admin');
  }
  const m = authority.members.find((x) => equal(zw.fingerprint(x.identity), next.signed.signer));
  if (!m) throw chainError('signer is not a member');
  try {
    zw.verifySignature(m.identity, zw.label('SIG_ACL'), next.docBytes, next.signed.sig);
  } catch {
    throw chainError(`bad signature on version ${next.version}`);
  }
}
