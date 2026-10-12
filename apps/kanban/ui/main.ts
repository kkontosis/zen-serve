// The kanban app's page: a board's columns and cards, drag and drop (and
// buttons) to move them, attachments, and a card's long text (Loro).
// Changes from elsewhere arrive as change events and re-render the board;
// "dragging…" cursors go over the board's ephemeral topic.
//
// `?board=<id>` picks the board (default "family"); `?crdt=1` shows the
// offline variant, cards in a CRDT table (src/crdt.ts).
import { bytes } from '@zen/client';
import type { LoroHandle } from '@zen/loro';
import { boot, h, message, ready, render } from '../../shared/boot.js';
import {
  type CrdtCard,
  crdtAddCard,
  crdtColumnCards,
  crdtDelete,
  crdtEdit,
  crdtLabel,
  crdtMove,
  crdtVote,
  watchCrdtCards,
} from '../src/crdt.js';
import {
  type Attachment,
  addCard,
  attachFile,
  boardTree,
  type Card,
  type Column,
  columnCards,
  DB_OPTIONS,
  deleteCard,
  editCard,
  ensureBoard,
  listColumns,
  moveCard,
  NS,
  onCursors,
  openCardText,
  publishCursor,
  readAttachment,
  watchCards,
} from '../src/kanban.js';

const params = new URLSearchParams(location.search);
const boardId = params.get('board') ?? 'family';
const crdt = params.get('crdt') === '1';

const ctx = await boot({ app: 'kanban', ns: NS, db: DB_OPTIONS });
const { db, root, user } = ctx;
const board = await ensureBoard(db, boardId, 'Family board');
const tree = boardTree(ctx.fs, board);

/** What is shown: each column with its cards (either variant). */
type ViewCard = Card | CrdtCard;
let columns: Column[] = [];
let view: { column: Column; cards: ViewCard[] }[] = [];
let selected: string | undefined;
const cursors = new Map<string, string>();
let error = '';

const boardEl = h('div', { id: 'board' });
const detailEl = h('aside', { id: 'detail' });
const cursorsEl = h('p', { id: 'cursors' });
const errorEl = h('p', { id: 'error', class: 'zen-error' });
const newTitle = h('input', { id: 'new-title', placeholder: 'a new card' });

const fail = (e: unknown) => {
  error = message(e);
  errorEl.textContent = error;
  console.error(e);
};
const run = (fn: () => Promise<unknown>) => () => void fn().catch(fail);

// ------------------------------------------------------------------ data

async function load(): Promise<void> {
  columns = await listColumns(db, board.id);
  view = await Promise.all(
    columns.map(async (column) => ({
      column,
      cards: crdt ? await crdtCards(column.id) : await columnCards(db, column.id),
    })),
  );
}

/**
 * The CRDT cards of a column: merged on the server while it can be reached
 * (a change event means the cached state may be old), else as cached.
 */
async function crdtCards(column: string): Promise<CrdtCard[]> {
  try {
    return await crdtColumnCards(db, column, 'server');
  } catch {
    return crdtColumnCards(db, column, 'cache');
  }
}

/**
 * The CRDT index follows a change a moment later (its indexer consumes
 * the same events): render now and a few times over the next seconds.
 */
function refreshLagging(): void {
  void refresh();
  for (const ms of [300, 1000, 2500]) setTimeout(refresh, ms);
}

/** Re-render from the database; calls made while one runs are folded into one more. */
let loading: Promise<void> | undefined;
let again = false;
function refresh(): Promise<void> {
  if (loading) {
    again = true;
    return loading;
  }
  loading = (async () => {
    try {
      do {
        again = false;
        await load();
      } while (again);
      show();
    } catch (e) {
      fail(e);
    } finally {
      loading = undefined;
    }
  })();
  return loading;
}

const cardOf = (id: string) => view.flatMap((v) => v.cards).find((c) => c.id === id);
const placeOf = (id: string) => {
  for (const [ci, v] of view.entries()) {
    const i = v.cards.findIndex((c) => c.id === id);
    if (i >= 0) return { ci, i };
  }
  return undefined;
};

/** Move a card to `column` at `index` among the other cards there (default: the end). */
async function move(id: string, column: string, index?: number): Promise<void> {
  if (crdt) {
    const others = view.find((v) => v.column.id === column)!.cards.filter((c) => c.id !== id);
    const i = index ?? others.length;
    await crdtMove(db, id, column, others[i - 1]?.pos, others[i]?.pos);
    refreshLagging();
  } else {
    await moveCard(db, id, column, index);
  }
  await refresh();
}

async function moveBy(id: string, step: number): Promise<void> {
  const at = placeOf(id);
  const to = at && view[at.ci + step];
  if (to) await move(id, to.column.id);
}

// ------------------------------------------------------------------ rendering

const kb = (n: number) => (n < 1024 ? `${n} B` : `${Math.round(n / 1024)} KiB`);

function cardEl(c: ViewCard) {
  const el = h(
    'li',
    {
      class: `card${c.id === selected ? ' selected' : ''}`,
      'data-id': c.id,
      draggable: 'true',
      ondragstart: ((e: DragEvent) => {
        e.dataTransfer?.setData('text/plain', c.id);
        dragging = true;
        void publishCursor(db, board.id, { user, card: c.id }).catch(() => {});
      }) as EventListener,
      ondragend: () => {
        dragging = false;
        show();
        void publishCursor(db, board.id, { user, card: null }).catch(() => {});
      },
      onclick: () => select(c.id),
    },
    h('span', { class: 'title' }, c.title),
    'votes' in c ? h('span', { class: 'votes' }, ` ▲${c.votes}`) : null,
    'labels' in c && c.labels.length
      ? h('span', { class: 'labels' }, ` [${[...c.labels].sort().join(', ')}]`)
      : null,
    'attachments' in c && c.attachments.length
      ? h('span', { class: 'clip' }, ` 📎${c.attachments.length}`)
      : null,
    h(
      'span',
      { class: 'actions' },
      h(
        'button',
        { class: 'act move-left', title: 'move left', onclick: stop(() => moveBy(c.id, -1)) },
        '←',
      ),
      h(
        'button',
        { class: 'act move-right', title: 'move right', onclick: stop(() => moveBy(c.id, 1)) },
        '→',
      ),
    ),
  );
  return el;
}

/** A button handler that doesn't also select the card. */
const stop = (fn: () => Promise<unknown>) => (e: Event) => {
  e.stopPropagation();
  void fn().catch(fail);
};

function columnEl(v: { column: Column; cards: ViewCard[] }) {
  const list = h('ul', { class: 'cards' }, v.cards.map(cardEl));
  return h(
    'section',
    {
      class: 'column',
      'data-id': v.column.id,
      ondragover: (e: Event) => e.preventDefault(),
      ondrop: ((e: DragEvent) => {
        e.preventDefault();
        const id = e.dataTransfer?.getData('text/plain');
        if (!id) return;
        // Dropped on a card: before it; elsewhere in the column: at the end.
        const over = (e.target as Element).closest('.card') as HTMLElement | null;
        const others = v.cards.filter((c) => c.id !== id);
        const i = over ? others.findIndex((c) => c.id === over.dataset.id) : -1;
        void move(id, v.column.id, i >= 0 ? i : undefined).catch(fail);
      }) as EventListener,
    },
    h('h2', {}, v.column.name),
    list,
  );
}

/** While a card is dragged its element must stay: the board renders at the drag's end. */
let dragging = false;

function show(): void {
  if (!dragging) render(boardEl, view.map(columnEl));
  const names = [...cursors.entries()].map(([who, title]) => `${who} is dragging “${title}”…`);
  cursorsEl.textContent = names.join(' ');
  if (selected && !cardOf(selected)) void select(undefined);
  else showDetail();
}

// ------------------------------------------------------------------ the selected card

let text: LoroHandle | undefined;
const textArea = h('textarea', { id: 'card-text', placeholder: 'the card text' });

/** Apply the textarea's edit to the Loro text as one delete and one insert. */
textArea.addEventListener('input', () => {
  if (!text) return;
  const t = text.doc.getText('text');
  const old = t.toString();
  const now = textArea.value;
  let p = 0;
  while (p < old.length && p < now.length && old[p] === now[p]) p++;
  let s = 0;
  while (
    s < old.length - p &&
    s < now.length - p &&
    old[old.length - 1 - s] === now[now.length - 1 - s]
  )
    s++;
  if (old.length - p - s > 0) t.delete(p, old.length - p - s);
  if (now.length - p - s > 0) t.insert(p, now.slice(p, now.length - s));
  text.doc.commit();
});

/** Show a remote edit, keeping the caret where it was in the text. */
function showText(): void {
  if (!text) return;
  const old = textArea.value;
  const now = text.doc.getText('text').toString();
  if (old === now) return;
  let p = 0;
  while (p < old.length && p < now.length && old[p] === now[p]) p++;
  const shift = (x: number) => (x > p ? Math.max(p, x + now.length - old.length) : x);
  const [a, b] = [textArea.selectionStart, textArea.selectionEnd];
  textArea.value = now;
  if (document.activeElement === textArea) textArea.setSelectionRange(shift(a), shift(b));
}

async function select(id: string | undefined): Promise<void> {
  if (id === selected && (text || !id)) return showDetail();
  selected = id;
  const old = text;
  text = undefined;
  await old?.close();
  textArea.value = '';
  if (id) {
    const h = await openCardText(db, id);
    if (selected !== id) {
      await h.close();
      return;
    }
    text = h;
    h.onChange(showText);
    showText();
  }
  show();
}

// The panel's controls are made once and kept across renders, so a render
// caused by someone else's change never drops what is being typed.
const titleInput = h('input', { id: 'card-title' });
let titleShown = '';
const titleForm = h(
  'form',
  {
    onsubmit: (e: Event) => {
      e.preventDefault();
      const id = selected;
      if (!id) return;
      const title = titleInput.value;
      void (async () => {
        if (crdt) await crdtEdit(db, id, { title });
        else await editCard(db, id, { title });
        await (crdt ? refreshLagging() : refresh());
      })().catch(fail);
    },
  },
  titleInput,
  h('button', { id: 'card-rename', type: 'submit' }, 'Rename'),
);
const deleteButton = h(
  'button',
  {
    id: 'card-delete',
    onclick: run(async () => {
      const id = selected;
      if (!id) return;
      if (crdt) await crdtDelete(db, id);
      else await deleteCard(db, tree, id);
      await select(undefined);
      await refresh();
    }),
  },
  'Delete card',
);
const attachInput = h('input', { id: 'attach-input', type: 'file' });
attachInput.addEventListener('change', async () => {
  const f = attachInput.files?.[0];
  const id = selected;
  if (!f || !id) return;
  try {
    await attachFile(db, tree, id, f.name, new Uint8Array(await f.arrayBuffer()));
    attachInput.value = '';
    await refresh();
  } catch (e) {
    fail(e);
  }
});
const voteButton = h('button', {
  id: 'card-vote',
  onclick: run(async () => {
    if (selected) await crdtVote(db, selected);
    refreshLagging();
  }),
});
const labelInput = h('input', { id: 'label-text', placeholder: 'a label' });
const labelForm = h(
  'form',
  {
    onsubmit: (e: Event) => {
      e.preventDefault();
      const label = labelInput.value.trim();
      if (!selected || !label) return;
      labelInput.value = '';
      void crdtLabel(db, selected, label).then(refreshLagging).catch(fail);
    },
  },
  labelInput,
  h('button', { id: 'label-add', type: 'submit' }, 'Add label'),
);

let detailFor: string | undefined;
function showDetail(): void {
  // Re-rendering moves the controls: keep the focus and the caret.
  const el = document.activeElement;
  const field =
    el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement ? el : undefined;
  const [a, b] = [field?.selectionStart ?? null, field?.selectionEnd ?? null];
  renderDetail();
  if (field && field !== document.activeElement && detailEl.contains(field)) {
    field.focus();
    if (a !== null && b !== null && field.type !== 'file') field.setSelectionRange(a, b);
  }
}

function renderDetail(): void {
  const c = selected ? cardOf(selected) : undefined;
  if (!c) {
    detailFor = undefined;
    render(detailEl, h('p', { class: 'hint' }, 'Select a card.'));
    return;
  }
  // The title field follows the card unless it is being edited.
  if (detailFor !== c.id) labelInput.value = '';
  if (detailFor !== c.id || titleInput.value === titleShown) titleInput.value = c.title;
  titleShown = c.title;
  detailFor = c.id;
  const common = [h('h2', {}, c.title), titleForm, h('label', {}, 'Text'), textArea, deleteButton];
  if ('attachments' in c) {
    render(
      detailEl,
      ...common,
      h('h3', {}, 'Attachments'),
      h('ul', { id: 'attachments' }, c.attachments.map(attachmentEl)),
      attachInput,
    );
  } else {
    voteButton.textContent = `▲ ${c.votes}`;
    render(detailEl, ...common, voteButton, labelForm);
  }
}

function attachmentEl(a: Attachment) {
  return h(
    'li',
    { class: 'attachment' },
    h(
      'a',
      {
        href: '#',
        onclick: (e: Event) => {
          e.preventDefault();
          void download(a).catch(fail);
        },
      },
      a.name,
    ),
    ` (${kb(a.size)})`,
  );
}

async function download(a: Attachment): Promise<void> {
  const data = await readAttachment(tree, a);
  const url = URL.createObjectURL(new Blob([new Uint8Array(data)]));
  h('a', { href: url, download: a.name }).click();
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
}

// ------------------------------------------------------------------ live

const offsetKey = `kanban:${user}:${board.id}:offset`;
function lastOffset(): Uint8Array | undefined {
  try {
    const v = localStorage.getItem(offsetKey);
    return v ? bytes.fromHex(v) : undefined;
  } catch {
    return undefined;
  }
}

let events = 0;
if (crdt) {
  await watchCrdtCards(db, () => {
    events++;
    refreshLagging();
  });
} else {
  // From the last offset this device saw: what happened while it was away, then live.
  await watchCards(
    db,
    board.id,
    (m) => {
      events++;
      try {
        localStorage.setItem(offsetKey, bytes.hex(m.offset));
      } catch {}
      void refresh();
    },
    lastOffset(),
  );
}
await onCursors(db, board.id, (m) => {
  const { user: who, card } = m.body;
  if (who === user) return;
  if (card) cursors.set(who, cardOf(card)?.title ?? '…');
  else cursors.delete(who);
  show();
});

render(
  root,
  h('h1', {}, board.name, crdt ? ' (offline variant)' : ''),
  h(
    'form',
    {
      id: 'new-card',
      onsubmit: (e: Event) => {
        e.preventDefault();
        void (async () => {
          const first = columns[0];
          if (!first || !newTitle.value) return;
          if (crdt) {
            const last = view[0]?.cards.at(-1);
            await crdtAddCard(db, board.id, first.id, newTitle.value, last?.pos);
            refreshLagging();
          } else {
            await addCard(db, tree, first, newTitle.value);
          }
          newTitle.value = '';
          await refresh();
        })().catch(fail);
      },
    },
    newTitle,
    h('button', { id: 'new-add', type: 'submit' }, 'Add card'),
  ),
  cursorsEl,
  errorEl,
  h('div', { id: 'layout' }, boardEl, detailEl),
);
await refresh();
ready(ctx, {
  board,
  crdt,
  view: () => view,
  events: () => events,
  error: () => error,
  text: () => text?.doc.getText('text').toString(),
  refresh,
  select,
});
