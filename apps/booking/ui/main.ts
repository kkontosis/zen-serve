// The booking app's page: resources and their slots, book / cancel / join
// the waitlist, the user's bookings and notifications. The waitlist
// promotion runs here too (a consumer runs in the owner tab).
import { boot, h, message, ready, render } from '../../shared/boot.js';
import {
  addSlot,
  type Booking,
  book,
  bookingsOf,
  cancel,
  DB_OPTIONS,
  HOUR,
  joinWaitlist,
  listResources,
  listSlots,
  NS,
  onNotify,
  onReleased,
  type Slot,
  startPromotion,
  type WaitEntry,
  waitlistOf,
} from '../src/booking.js';

const ctx = await boot({ app: 'booking', ns: NS, db: DB_OPTIONS });
const { db, root, user } = ctx;

startPromotion(db);

interface View {
  resource: string;
  slots: { slot: Slot; booking?: Booking; waitlist: WaitEntry[] }[];
}

const notices: string[] = [];
const error = h('p', { id: 'error', class: 'zen-error' });
const board = h('div', { id: 'board' });
const mine = h('ul', { id: 'my-bookings' });
const noticeList = h('ul', { id: 'notices' });

const when = (ms: number) =>
  new Date(ms).toLocaleString([], { dateStyle: 'short', timeStyle: 'short' });

/** Run an action, show its error, refresh. */
const act = (fn: () => Promise<unknown>) => async () => {
  render(error);
  try {
    await fn();
  } catch (e) {
    render(error, message(e));
  }
  await refresh();
};

function slotItem(v: View['slots'][number]): HTMLElement {
  const { slot, booking, waitlist } = v;
  const place = waitlist.findIndex((e) => e.who === user);
  let status: string;
  const buttons: HTMLElement[] = [];
  if (!booking) {
    status = 'free';
    buttons.push(
      h('button', { class: 'book', onclick: act(() => book(db, slot.id, user)) }, 'Book'),
    );
  } else if (booking.who === user) {
    status = 'yours';
    buttons.push(
      h('button', { class: 'cancel', onclick: act(() => cancel(db, slot.id, user)) }, 'Cancel'),
    );
  } else {
    status = `booked by ${booking.who}`;
    if (place < 0) {
      buttons.push(
        h(
          'button',
          { class: 'join', onclick: act(() => joinWaitlist(db, slot.id, user)) },
          'Join waitlist',
        ),
      );
    }
  }
  const wait =
    place >= 0
      ? `on the waitlist (#${place + 1} of ${waitlist.length})`
      : waitlist.length
        ? `${waitlist.length} waiting`
        : '';
  return h(
    'li',
    { class: 'slot', 'data-slot': slot.id, 'data-resource': slot.resource },
    h('span', { class: 'when' }, `${when(slot.start)}–${when(slot.end)}`),
    ' ',
    h('span', { class: 'status' }, status),
    ' ',
    h('span', { class: 'wait' }, wait),
    ' ',
    buttons,
  );
}

let shown = '';
let running: Promise<void> | undefined;
let queued: Promise<void> | undefined;

/** Read the board and re-render what changed (once more after a read in progress). */
function refresh(): Promise<void> {
  if (!running) {
    running = load().finally(() => {
      running = undefined;
    });
    return running;
  }
  queued ??= running.then(() => {
    queued = undefined;
    return refresh();
  });
  return queued;
}

async function load(): Promise<void> {
  try {
    const views: View[] = [];
    for (const resource of await listResources(db)) {
      const slots = await listSlots(db, resource);
      const booked = await bookingsOf(
        db,
        slots.map((s) => s.id),
      );
      const waits = await Promise.all(slots.map((s) => waitlistOf(db, s.id)));
      views.push({
        resource,
        slots: slots.map((slot, i) => ({
          slot,
          booking: booked.get(slot.id),
          waitlist: waits[i]!,
        })),
      });
    }
    const sig = JSON.stringify(views, (_k, v) => (v instanceof Uint8Array ? undefined : v));
    if (sig === shown) return;
    shown = sig;
    render(
      board,
      views.map((v) =>
        h(
          'section',
          { class: 'resource' },
          h('h3', {}, v.resource),
          h('ul', {}, v.slots.map(slotItem)),
        ),
      ),
    );
    render(
      mine,
      views.flatMap((v) =>
        v.slots
          .filter((s) => s.booking?.who === user)
          .map((s) => h('li', { class: 'mine' }, `${v.resource} · ${when(s.slot.start)}`)),
      ),
    );
  } catch (e) {
    render(error, message(e));
  }
}

const showNotices = () =>
  render(
    noticeList,
    notices.map((n) => h('li', { class: 'notice' }, n)),
  );

await onNotify(
  db,
  (m) => {
    const what = m.type === 'promoted' ? 'promoted from the waitlist' : 'reminder';
    notices.unshift(`${what}: ${m.body.resource} · ${when(m.body.start)}`);
    showNotices();
    void refresh();
  },
  user,
);
await onReleased(db, () => void refresh());

// The add-slot form, starting at the next whole hour plus two.
const resource = h('input', { id: 'slot-resource', placeholder: 'resource (a court, a car)' });
const startAt = new Date(Math.ceil(Date.now() / HOUR) * HOUR + 2 * HOUR);
const local = new Date(startAt.getTime() - startAt.getTimezoneOffset() * 60_000);
const start = h('input', {
  id: 'slot-start',
  type: 'datetime-local',
  value: local.toISOString().slice(0, 16),
});
const minutes = h('input', { id: 'slot-minutes', type: 'number', value: 60, min: 1 });

render(
  root,
  h(
    'form',
    {
      id: 'slot-form',
      onsubmit: (e: Event) => {
        e.preventDefault();
        void act(async () => {
          const s = new Date(start.value).getTime();
          if (Number.isNaN(s)) throw new Error('pick a start time');
          await addSlot(db, resource.value.trim(), s, s + Number(minutes.value) * 60_000);
        })();
      },
    },
    resource,
    start,
    minutes,
    h('button', { id: 'slot-add', type: 'submit' }, 'Add slot'),
  ),
  error,
  board,
  h('h2', {}, 'My bookings'),
  mine,
  h('h2', {}, 'Notifications'),
  noticeList,
);

await refresh();
// Other members' bookings emit nothing: poll for them.
setInterval(() => void refresh(), 1000);
ready(ctx, { notices, refresh });
