// The mail app's page: the signed-in user's addresses and inbox (live, as
// mail arrives and as IMAP clients change it), a message view (opening a
// message marks it read), flag, unread and delete buttons, and the app
// passwords IMAP clients log in with. Changes made here reach idling IMAP
// sessions through `mail/changed`.
import { boot, h, message, ready, render } from '../../shared/boot.js';
import { changedTopic, DB_OPTIONS, type Message, NS, receivedTopic } from '../src/schema.js';
import {
  addressesOf,
  appPasswords,
  createAppPassword,
  expunge,
  messageBytes,
  messages,
  revokeAppPassword,
  setFlags,
} from '../src/store.js';

const ctx = await boot({ app: 'mail', ns: NS, db: DB_OPTIONS });
const { db, root, user } = ctx;

const addresses = h('p', { id: 'addresses' });
const inbox = h('ul', { id: 'inbox' });
const view = h('div', { id: 'view' });
const passwords = h('ul', { id: 'passwords' });
const shown = h('p', {});
const errors = h('p', { class: 'zen-error', id: 'error' });

/** The message open in the view, by uid. */
let open: number | undefined;
let rows: Message[] = [];

const fail = (e: unknown) => {
  errors.textContent = message(e);
};

const date = (ms: number) => new Date(ms).toISOString().slice(0, 16).replace('T', ' ');

async function showAddresses(): Promise<void> {
  const mine = await addressesOf(db, user);
  addresses.textContent = mine.length
    ? `Your addresses: ${mine.join(', ')}`
    : 'No address delivers here yet.';
}

async function showInbox(): Promise<void> {
  rows = (await messages(db, user)).reverse();
  render(
    inbox,
    rows.map((m) =>
      h(
        'li',
        {
          class: `msg${m.flags.includes('\\Seen') ? '' : ' unread'}${m.uid === open ? ' open' : ''}`,
          'data-uid': String(m.uid),
          onclick: () => void openMessage(m.uid).catch(fail),
        },
        h('span', { class: 'star' }, m.flags.includes('\\Flagged') ? '★' : ''),
        h('span', { class: 'from' }, m.fields?.from || m.from || '(no sender)'),
        h('span', { class: 'subject' }, m.fields?.subject || '(no subject)'),
        h('span', { class: 'date' }, date(m.received)),
        h('span', { class: 'size' }, `${Math.ceil(m.size / 1024)} KB`),
      ),
    ),
  );
  if (open !== undefined && !rows.some((m) => m.uid === open)) {
    open = undefined;
    render(view);
  }
}

/** Change the open message's flags. */
const flags = (op: 'add' | 'remove', f: string) => async () => {
  if (open === undefined) return;
  const uid = open;
  await db.transaction((tx) => setFlags(tx, user, [uid], op, [f]));
  await showInbox();
  await showView();
};

async function showView(): Promise<void> {
  const m = rows.find((r) => r.uid === open);
  if (!m) return render(view);
  const flagged = m.flags.includes('\\Flagged');
  const text = new TextDecoder().decode(await messageBytes(db, user, m.uid));
  render(
    view,
    h(
      'p',
      {},
      h(
        'button',
        {
          id: 'flag',
          onclick: () => void flags(flagged ? 'remove' : 'add', '\\Flagged')().catch(fail),
        },
        flagged ? 'Unflag' : 'Flag',
      ),
      ' ',
      h('button', { id: 'unread', onclick: () => void markUnread().catch(fail) }, 'Mark unread'),
      ' ',
      h('button', { id: 'delete', onclick: () => void remove().catch(fail) }, 'Delete'),
    ),
    h('pre', { id: 'message' }, text),
  );
}

async function openMessage(uid: number): Promise<void> {
  open = uid;
  const m = rows.find((r) => r.uid === uid);
  if (m && !m.flags.includes('\\Seen')) {
    await db.transaction((tx) => setFlags(tx, user, [uid], 'add', ['\\Seen']));
  }
  await showInbox();
  await showView();
}

async function markUnread(): Promise<void> {
  await flags('remove', '\\Seen')();
  // Closed, so that it stays unread.
  open = undefined;
  render(view);
  await showInbox();
}

async function remove(): Promise<void> {
  if (open === undefined) return;
  const uid = open;
  open = undefined;
  await db.transaction((tx) => expunge(tx, user, [uid]));
  render(view);
  await showInbox();
}

async function showPasswords(): Promise<void> {
  const list = await appPasswords(db, user);
  render(
    passwords,
    list.map((p) =>
      h(
        'li',
        { class: 'pw', 'data-id': p.id },
        `${p.label} (made ${date(p.created)}) `,
        h(
          'button',
          {
            class: 'revoke',
            onclick: async () => {
              await revokeAppPassword(db, user, p.id).catch(fail);
              await showPasswords();
            },
          },
          'Revoke',
        ),
      ),
    ),
  );
}

const label = h('input', { id: 'pw-label', placeholder: 'A label (e.g. phone)' });
const create = h(
  'button',
  {
    id: 'pw-create',
    onclick: async () => {
      try {
        const p = await createAppPassword(db, user, label.value || 'app password');
        label.value = '';
        render(
          shown,
          'Your new app password, shown once: ',
          h('span', { id: 'new-password' }, p.password),
        );
        await showPasswords();
      } catch (e) {
        fail(e);
      }
    },
  },
  'Create',
);

render(
  root,
  addresses,
  errors,
  h('h2', {}, 'Inbox'),
  inbox,
  view,
  h('h2', {}, 'App passwords'),
  h('p', {}, 'IMAP clients log in with your name or address and an app password.'),
  h('p', {}, label, ' ', create),
  shown,
  passwords,
);
await Promise.all([showAddresses(), showInbox(), showPasswords()]);

// Live: new mail, and changes made elsewhere (IMAP clients, other tabs).
const mine = (m: { body: unknown }) => (m.body as { user?: string }).user === user;
const refresh = (m: { body: unknown }) => {
  if (!mine(m)) return;
  void showInbox()
    .then(() => showView())
    .catch(fail);
};
await db.on(receivedTopic(db), refresh);
await db.on(changedTopic(db), refresh);

ready(ctx, {
  inbox: () =>
    rows.map((m) => ({ uid: m.uid, subject: m.fields?.subject ?? '', flags: [...m.flags] })),
});
