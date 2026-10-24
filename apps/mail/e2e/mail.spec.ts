// The mail app in Chromium, with SMTP and IMAP served by Node workers.
// Alice's page makes an app password; mail sent through SMTP shows in the
// page live; an IMAP client logged in with that password idles, and what
// the page does (opening a message, deleting one) reaches it, as does new
// mail.
import { connect as tcp } from 'node:net';
import { connect, type TLSSocket } from 'node:tls';
import { expect, test } from '@playwright/test';
import { testCerts } from '../../../packages/client/src/testing/index.js';
import {
  openApp,
  spawnWorker,
  startWorld,
  until,
  type WorkerProcess,
  type World,
  workerEnv,
} from '../../shared/harness.js';
import { signInWorker } from '../../shared/node.js';
import { DB_OPTIONS, NS, putCert, putMailbox } from '../src/schema.js';
import { GRANTS } from '../test/grants.js';

let w: World;
let ca: string;
const workers: WorkerProcess[] = [];

test.beforeAll(async () => {
  w = await startWorld({
    users: ['alice'],
    grants: GRANTS,
    services: [
      { name: 'smtp', path: ['svc', 'smtp'] },
      { name: 'imaps', path: ['svc', 'imaps'], more: { idle_secs: 1860 } },
    ],
  });
  const alice = await signInWorker({
    url: w.url,
    device: w.user('alice').deviceFile,
    passphrase: w.passphrase,
  });
  try {
    const db = await alice.open(NS, { ...DB_OPTIONS, warn: () => {} });
    const c = testCerts();
    ca = c.ca;
    await putCert(db, c.chain, c.key);
    await putMailbox(db, 'alice@example.test', 'alice');
  } finally {
    await alice.close();
  }
  for (const name of ['smtp', 'imap']) {
    const k = spawnWorker('mail', name, workerEnv(w, 'alice'));
    workers.push(k);
    await k.waitFor('handler ready', 60_000);
  }
});

test.afterAll(async () => {
  for (const k of workers) await k.kill();
  await w?.stop();
});

/** Send one message through SMTP. */
async function send(subject: string): Promise<void> {
  const s = tcp(w.servicePort('smtp'), '127.0.0.1');
  let buf = '';
  s.on('data', (d: Buffer) => {
    buf += d.toString();
  });
  await new Promise<void>((ok, fail) => s.once('connect', ok).once('error', fail));
  try {
    await until(() => buf.includes('220 '), 30_000, 'the greeting');
    s.write('EHLO t\r\nMAIL FROM:<bob@remote.test>\r\nRCPT TO:<alice@example.test>\r\nDATA\r\n');
    await until(() => buf.includes('354 '), 30_000, `354 (${buf})`);
    s.write(`From: bob@remote.test\r\nSubject: ${subject}\r\n\r\nHello.\r\n.\r\nQUIT\r\n`);
    await until(() => buf.includes('221 '), 30_000, `the 250 and 221 (${buf})`);
  } finally {
    s.destroy();
  }
}

/** An IMAP client that keeps every response line. */
class Imap {
  lines: string[] = [];
  private buf = '';
  constructor(readonly s: TLSSocket) {
    s.on('data', (d: Buffer) => {
      this.buf += d.toString('latin1');
      for (let i = this.buf.indexOf('\r\n'); i >= 0; i = this.buf.indexOf('\r\n')) {
        this.lines.push(this.buf.slice(0, i));
        this.buf = this.buf.slice(i + 2);
      }
    });
    s.on('error', () => {});
  }

  async wait(line: string | RegExp, what = String(line)): Promise<void> {
    const ok = (l: string) => (typeof line === 'string' ? l === line : line.test(l));
    await until(() => this.lines.some(ok), 30_000, `${what} (have ${JSON.stringify(this.lines)})`);
  }

  async cmd(tag: string, text: string): Promise<void> {
    this.s.write(`${tag} ${text}\r\n`);
    await this.wait(new RegExp(`^${tag} OK`), `${tag} ${text}`);
  }
}

test('the inbox page: app passwords, live mail, and changes that reach an idling IMAP client', async ({
  page,
}) => {
  await openApp(page, w, 'mail', 'alice');
  await expect(page.locator('#addresses')).toContainText('alice@example.test');

  // 1. An app password, shown once.
  await page.fill('#pw-label', 'laptop');
  await page.click('#pw-create');
  const password = (await page.locator('#new-password').textContent())!;
  expect(password).toMatch(/^[a-z2-7]{8}(-[a-z2-7]{8}){3}$/);
  await expect(page.locator('#passwords .pw')).toContainText([/^laptop/]);

  // 2. Mail shows up live.
  await send('first');
  await expect(page.locator('#inbox .msg')).toHaveCount(1);
  await expect(page.locator('#inbox .msg.unread .subject')).toHaveText(['first']);

  // 3. An IMAP client logs in with the password and idles.
  const s = connect({
    host: '127.0.0.1',
    port: w.servicePort('imaps'),
    ca,
    servername: 'localhost',
  });
  await new Promise<void>((ok, fail) => s.once('secureConnect', ok).once('error', fail));
  const c = new Imap(s);
  try {
    await c.wait(/^\* OK/);
    await c.cmd('a', `LOGIN alice@example.test ${password}`);
    await c.cmd('b', 'SELECT INBOX');
    expect(c.lines).toContain('* 1 EXISTS');
    s.write('i IDLE\r\n');
    await c.wait('+ idling');

    // Opening the message in the page marks it read: the client is told.
    await page.click('#inbox .msg[data-uid="1"]');
    await expect(page.locator('#message')).toContainText('Subject: first');
    await expect(page.locator('#inbox .msg.unread')).toHaveCount(0);
    await c.wait('* 1 FETCH (UID 1 FLAGS (\\Seen))');

    // 4. New mail: the page shows it, and the client gets EXISTS.
    await send('second');
    await expect(page.locator('#inbox .msg')).toHaveCount(2);
    await c.wait('* 2 EXISTS');

    // 5. Deleting the first in the page: the client gets its EXPUNGE.
    await page.click('#inbox .msg[data-uid="1"]');
    await page.click('#delete');
    await expect(page.locator('#inbox .msg')).toHaveCount(1);
    await c.wait('* 1 EXPUNGE');
    expect(
      await page.evaluate(() =>
        (window as unknown as { app: { inbox(): { subject: string }[] } }).app.inbox(),
      ),
    ).toEqual([{ uid: 2, subject: 'second', flags: [] }]);

    s.write('DONE\r\n');
    await c.wait('i OK IDLE terminated');
    for (const k of workers) {
      expect(k.output.filter((l) => l.startsWith('error'))).toEqual([]);
    }
  } finally {
    s.destroy();
  }
});
