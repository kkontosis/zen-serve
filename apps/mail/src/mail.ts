// The mail app: inbound SMTP into zen-db (smtp.ts), and what reads it
// back and changes it: IMAP (imap.ts) and the inbox page, which share the
// helpers of store.ts. SMTP is `svc/smtp`, served with `starttls()`, and
// IMAP `svc/imaps`, with `tls()`, both with the certificate of `certs`;
// with a sealed inbox (svc.md §9), SMTP's seed is in `certs` too.
import type { Db } from '@zen/db';
import { type Layer, type ServeHandle, type ServeOptions, serve, starttls, tls } from '@zen/svc';
import { type ImapOptions, imap } from './imap.js';
import { type Cert, type Draft, type Part, TLS_CERT } from './schema.js';
import { type SmtpOptions, smtp } from './smtp.js';

export { headerLines, parseFields, scanHeaderEnd } from './header.js';
export { CAPABILITIES, type ImapOptions, imap, MAX_COMMAND } from './imap.js';
export * from './schema.js';
export { decodeData, MAX_LINE, MAX_RCPTS, MAX_SIZE, type SmtpOptions, smtp } from './smtp.js';
export * from './store.js';

/** The inbound SMTP service. */
export const SMTP_SERVICE = ['svc', 'smtp'];

/** The IMAP service: implicit TLS (RFC 8314), port 993 in production. */
export const IMAP_SERVICE = ['svc', 'imaps'];

/** `starttls()` with the certificate of `certs`. */
export async function smtpLayer(db: Db): Promise<Layer> {
  const c = await db.table<Cert>('certs').get(TLS_CERT);
  if (!c) throw new Error(`no certificate ${TLS_CERT} in certs`);
  return starttls({ cert: c.chain, key: c.key });
}

/** Serve inbound SMTP from this database. */
export async function serveSmtp(
  db: Db,
  opts: Partial<ServeOptions> & { smtp: SmtpOptions },
): Promise<ServeHandle> {
  const { smtp: o, ...rest } = opts;
  return serve(db, {
    service: SMTP_SERVICE,
    layer: await smtpLayer(db),
    ...rest,
    handler: smtp(o),
  });
}

/**
 * Delete the drafts older than `olderThanMs`, with their parts: what a
 * connection that died mid-DATA left (a peer that went away cleanly gets
 * its draft deleted at once). Returns how many it deleted.
 */
export async function sweepDrafts(db: Db, olderThanMs: number): Promise<number> {
  const before = Date.now() - olderThanMs;
  const old = (await db.table<Draft>('drafts').query().all()).filter((d) => d.started <= before);
  for (const d of old) {
    const seqs = (
      await db.table<Part>('parts').query().where('draft', '=', d.draft).orderBy('seq').all()
    ).map((p) => p.seq);
    await db.transaction(async (tx) => {
      for (const seq of seqs) await tx.table<Part>('parts').delete([d.draft, seq]);
      await tx.table<Draft>('drafts').delete(d.draft);
    });
  }
  return old.length;
}

/** `tls()` with the certificate of `certs` (the one SMTP uses). */
export async function imapLayer(db: Db): Promise<Layer> {
  const c = await db.table<Cert>('certs').get(TLS_CERT);
  if (!c) throw new Error(`no certificate ${TLS_CERT} in certs`);
  return tls({ cert: c.chain, key: c.key });
}

/**
 * Serve IMAP from this database. Idling sessions are woken by
 * `wakeOnMail(db, handle)`, which the caller runs next to it.
 */
export async function serveImap(
  db: Db,
  opts: Partial<ServeOptions> & { imap?: ImapOptions } = {},
): Promise<ServeHandle> {
  const { imap: o, ...rest } = opts;
  return serve(db, {
    service: IMAP_SERVICE,
    layer: await imapLayer(db),
    ...rest,
    handler: imap(o),
  });
}
