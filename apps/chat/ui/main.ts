// The chat app's page: rooms, the messages of the open room (history, then
// live), a composer with a photo, a typing line, "who's online" and
// reactions. Plain DOM; the logic is in src/chat.ts.
import { bytes } from '@zen/client';
import type { OnSubscription, SourceSub } from '@zen/db';
import { boot, h, message, ready, render } from '../../shared/boot.js';
import {
  type ChatMsg,
  createRoom,
  DB_OPTIONS,
  EMOJIS,
  type Emoji,
  join,
  listRooms,
  markRead,
  NS,
  onReactions,
  onRooms,
  onTyping,
  photosTree,
  type Room,
  react,
  reactions,
  readPhoto,
  readRoom,
  START,
  sendMessage,
  sendPhoto,
  servePresence,
  showFor,
  TYPING_SHOW_MS,
  Typing,
  whoIsOnline,
} from '../src/chat.js';

const ctx = await boot({ app: 'chat', ns: NS, db: DB_OPTIONS });
const { db, user, root } = ctx;
const tree = await photosTree(ctx.fs);

/** The room everyone is in at first. */
const FAMILY = 'family';

const roomList = h('ul', { id: 'rooms' });
const list = h('ul', { id: 'messages' });
const typingLine = h('div', { id: 'typing' });
const onlineList = h('span', { id: 'online-list' });
const errorLine = h('div', { id: 'error', class: 'zen-error' });
const input = h('input', { id: 'msg-text', placeholder: 'a message', autocomplete: 'off' });
const photo = h('input', { id: 'photo', type: 'file', accept: 'image/*' });
const roomName = h('input', { id: 'room-name', placeholder: 'new room' });

// ------------------------------------------------------------ the open room

let room = FAMILY;
/** The messages shown, in order (tests read them). */
let shown: ChatMsg[] = [];
/** The last offset shown: where reading resumes after going offline. */
let last: Uint8Array = START;
let sub: OnSubscription | undefined;
let typingSub: SourceSub | undefined;
/** Bumped on each (re)subscription: a stale subscription's events are dropped. */
let gen = 0;
let typing = new Typing(db, room, user);

const fail = (e: unknown) => render(errorLine, message(e));

/** Receipts are written behind the display, one at a time, the newest only. */
let marking = false;
let toMark: { room: string; offset: Uint8Array } | undefined;
async function markShown(): Promise<void> {
  if (marking) return;
  marking = true;
  try {
    while (toMark) {
      const m = toMark;
      toMark = undefined;
      await markRead(db, m.room, m.offset).catch(() => {});
    }
  } finally {
    marking = false;
  }
}

function msgItem(m: ChatMsg): HTMLLIElement {
  const counts = h('span', { class: 'reactions' });
  const li = h(
    'li',
    { class: 'msg', 'data-id': m.id },
    h('b', {}, m.from),
    ' ',
    h('span', { class: 'text' }, m.text),
    m.reply_to ? h('span', { class: 'reply' }, ' ↩') : null,
    counts,
  );
  if (m.photo) {
    const img = h('img', { class: 'photo', alt: 'photo' });
    li.append(img);
    readPhoto(db, tree, m.photo)
      .then((b) => {
        img.src = URL.createObjectURL(new Blob([b as BlobPart]));
      })
      .catch(fail);
  }
  void showReactions(m.id, counts);
  return li;
}

/**
 * Draw a message's reaction buttons. After a change event, `fresh` reads
 * the server: a cached state younger than `crdtCacheMs` would hide it.
 */
async function showReactions(id: string, el?: Element | null, fresh = false): Promise<void> {
  el ??= list.querySelector(`li[data-id="${id}"] .reactions`);
  if (!el) return;
  const r = await (fresh ? reactions(db, id, 'server') : Promise.reject())
    .catch(() => reactions(db, id))
    .catch(() => undefined);
  if (!r) return;
  render(
    el,
    (Object.keys(EMOJIS) as Emoji[]).map((e) =>
      h(
        'button',
        {
          class: 'react',
          'data-emoji': e,
          title: e,
          onclick: () => {
            react(db, id, e)
              .then(() => showReactions(id))
              .catch(fail);
          },
        },
        `${EMOJIS[e]} ${r[e] || ''}`,
      ),
    ),
  );
}

function onMsg(g: number, m: ChatMsg): void {
  if (g !== gen) return;
  shown.push(m);
  last = m.offset;
  list.append(msgItem(m));
  list.scrollTop = list.scrollHeight;
  toMark = { room: m.room, offset: m.offset };
  void markShown();
}

/** Follow the open room after `last`: the history, then live messages. */
async function follow(): Promise<void> {
  await stop();
  const g = ++gen;
  const s = await readRoom(db, room, (m) => onMsg(g, m), { after: last });
  if (g === gen) sub = s;
  else await s.close();
}

async function stop(): Promise<void> {
  gen++;
  const s = sub;
  sub = undefined;
  await s?.close();
}

async function openRoom(id: string): Promise<void> {
  await stop();
  await typingSub?.close();
  room = id;
  shown = [];
  last = START;
  render(list);
  render(typingLine);
  typing = new Typing(db, room, user);
  typingSub = await onTyping(db, room, showTyping);
  await follow();
  await showRooms();
}

// The browser says it is offline: stop reading, and resume after the last
// message shown when it is back. The stream also resumes on its own when
// its socket drops; this covers a socket the browser keeps open (Chromium's
// offline mode leaves an open WebSocket up).
addEventListener('offline', () => void stop());
addEventListener('online', () => void follow().catch(fail));

// ------------------------------------------------------------ typing

// Each member typing, until their newest indicator's time is up.
const typists = new Map<string, { until: number; timer: ReturnType<typeof setTimeout> }>();
// The indicators this page sent and received (wall and corrected clock
// times), for the spec's failure messages.
const typingLog: Record<string, unknown>[] = [];
function showTyping(from: string, sentMs: number): void {
  if (from === user) return;
  const left = showFor(db, sentMs);
  typingLog.push({ from, sentMs, now: db.fs.session.clock.now(), wall: Date.now(), left });
  const until = sentMs + TYPING_SHOW_MS;
  const t = typists.get(from);
  // An older indicator, arriving late, doesn't shorten a newer one.
  if (left <= 0 || (t && t.until >= until)) return;
  if (t) clearTimeout(t.timer);
  typists.set(from, {
    until,
    timer: setTimeout(() => {
      typists.delete(from);
      drawTyping();
    }, left),
  });
  drawTyping();
}
function drawTyping(): void {
  const names = [...typists.keys()];
  render(typingLine, names.length ? `${names.join(', ')} typing…` : '');
}
input.addEventListener('input', () => {
  const wall = Date.now();
  void typing
    .notify(wall)
    .then((sent) => {
      if (sent) typingLog.push({ sent: true, wall, now: db.fs.session.clock.now() });
    })
    .catch(() => {});
});

// ------------------------------------------------------------ rooms

async function showRooms(): Promise<void> {
  const rooms: Room[] = await listRooms(db).catch(() => []);
  render(
    roomList,
    rooms.map((r) =>
      h(
        'li',
        {
          class: r.id === room ? 'room open' : 'room',
          'data-id': r.id,
          onclick: () => void openRoom(r.id).catch(fail),
        },
        r.name ?? r.id,
        h('small', {}, ` ${(r.members ?? []).join(', ')}`),
        (r.members ?? []).includes(user)
          ? null
          : h(
              'button',
              {
                class: 'join',
                onclick: (e: Event) => {
                  e.stopPropagation();
                  join(db, r.id, user).then(showRooms).catch(fail);
                },
              },
              'join',
            ),
      ),
    ),
  );
}

// ------------------------------------------------------------ the page

render(
  root,
  h(
    'aside',
    {},
    h('h2', {}, 'Rooms'),
    roomList,
    h(
      'form',
      {
        onsubmit: (e: Event) => {
          e.preventDefault();
          const name = roomName.value.trim();
          if (!name) return;
          roomName.value = '';
          const id = crypto.randomUUID();
          createRoom(db, id, name, user)
            .then(() => openRoom(id))
            .catch(fail);
        },
      },
      roomName,
      h('button', { id: 'room-add', type: 'submit' }, 'Add'),
    ),
    h(
      'p',
      {},
      h(
        'button',
        {
          id: 'who',
          onclick: () => {
            render(onlineList, '…');
            whoIsOnline(db)
              .then((ps) => {
                const names = [...new Set(ps.map((p) => p.user))].sort();
                render(onlineList, `${ps.length} online: ${names.join(', ')}`);
              })
              .catch(fail);
          },
        },
        "Who's online?",
      ),
      ' ',
      onlineList,
    ),
  ),
  h(
    'section',
    {},
    list,
    typingLine,
    h(
      'form',
      {
        id: 'composer',
        onsubmit: async (e: Event) => {
          e.preventDefault();
          const text = input.value;
          const file = photo.files?.[0];
          if (!text && !file) return;
          try {
            if (file) {
              const data = new Uint8Array(await file.arrayBuffer());
              await sendPhoto(db, tree, room, user, { name: file.name, data }, text);
              photo.value = '';
            } else {
              await sendMessage(db, room, user, text);
            }
            input.value = '';
            render(errorLine);
          } catch (err) {
            fail(err);
          }
        },
      },
      input,
      photo,
      h('button', { id: 'msg-send', type: 'submit' }, 'Send'),
    ),
    errorLine,
  ),
);

await createRoom(db, FAMILY, 'Family', user);
await servePresence(db, user);
await onRooms(db, () => void showRooms());
await onReactions(db, (id) => void showReactions(id, null, true));
await openRoom(FAMILY);

ready(ctx, {
  /** The texts shown in the open room, in order. */
  texts: () => shown.map((m) => m.text),
  /** The offsets shown (hex), to check for repeats. */
  offsets: () => shown.map((m) => bytes.hex(m.offset)),
  room: () => room,
  typists: () => [...typists.keys()],
  typingLog: () => typingLog,
  whoIsOnline: () => whoIsOnline(db),
});
