// The hello app's page: notes (a table and an event per note) and a shared
// Loro text. The template of the other apps.
import { boot, h, ready, render } from '../../shared/boot.js';
import { addNote, DB_OPTIONS, listNotes, type Note, NS, onNotes, openText } from '../src/hello.js';

const ctx = await boot({ app: 'hello', ns: NS, db: DB_OPTIONS });
const { db, root } = ctx;

const events: string[] = [];
const list = h('ul', { id: 'notes' });
const input = h('input', { id: 'note-text', placeholder: 'a note' });
const text = h('textarea', { id: 'shared-text' });

const show = (notes: Note[]) =>
  render(
    list,
    notes.map((n) => h('li', { class: 'note' }, n.text)),
  );

const doc = await openText(db);
const t = doc.doc.getText('text');
text.value = t.toString();
doc.onChange(() => {
  text.value = t.toString();
});
text.addEventListener('input', () => {
  // Replace the whole text: enough for a demo (a real editor sends deltas).
  t.delete(0, t.length);
  t.insert(0, text.value);
  doc.doc.commit();
});

await onNotes(db, async (m) => {
  events.push(m.body.text);
  show(await listNotes(db));
});

render(
  root,
  h(
    'form',
    {
      onsubmit: async (e: Event) => {
        e.preventDefault();
        await addNote(db, input.value);
        input.value = '';
      },
    },
    input,
    h('button', { id: 'note-add', type: 'submit' }, 'Add'),
  ),
  list,
  h('h2', {}, 'Shared text'),
  text,
);
show(await listNotes(db));
ready(ctx, { events, text: () => t.toString(), doc });
