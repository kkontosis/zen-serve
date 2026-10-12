// The ledger's page: accounts and balances, a transfer form, an account's
// paged history (newest first), its statement for this month, and this
// device's audit view. Every page also runs the statement projection.
import { boot, h, message, ready, render } from '../../shared/boot.js';
import {
  type Account,
  type AuditRow,
  DB_OPTIONS,
  entriesTopic,
  HistoryPager,
  listAccounts,
  listAudit,
  monthOf,
  NS,
  openAccount,
  runAuditView,
  runStatements,
  statement,
  statementsTopic,
  transfer,
} from '../src/ledger.js';

/** Entries per history page (`?page=n` to change it). */
const PAGE = Number(new URLSearchParams(location.search).get('page') ?? 10) || 10;

const ctx = await boot({ app: 'ledger', ns: NS, db: DB_OPTIONS });
const { db, root, user } = ctx;

let accounts: Account[] = [];
let pager: HistoryPager | undefined;

const accountsList = h('tbody', { id: 'accounts' });
const fromSel = h('select', { id: 'tr-from' });
const toSel = h('select', { id: 'tr-to' });
const histSel = h('select', { id: 'hist-account' });
const amount = h('input', { id: 'tr-amount', type: 'number', min: '1', placeholder: 'amount' });
const memo = h('input', { id: 'tr-memo', placeholder: 'memo' });
const trError = h('p', { id: 'tr-error', class: 'zen-error' });
const newId = h('input', { id: 'open-id', placeholder: 'account name' });
const deposit = h('input', {
  id: 'open-deposit',
  type: 'number',
  min: '0',
  placeholder: 'deposit',
});
const openError = h('p', { id: 'open-error', class: 'zen-error' });
const history = h('ul', { id: 'history' });
const histPage = h('span', { id: 'hist-page' });
const newer = h('button', { id: 'hist-newer', type: 'button' }, '← newer');
const older = h('button', { id: 'hist-older', type: 'button' }, 'older →');
const stmt = h('div', { id: 'statement' });
const audit = h('ul', { id: 'audit' });

const money = (n: number) => (n > 0 ? `+${n}` : String(n));
const time = (at: number) => new Date(at).toLocaleTimeString();

function options(sel: HTMLSelectElement) {
  const keep = sel.value;
  render(
    sel,
    accounts.map((a) => h('option', { value: a.id }, a.id)),
  );
  if (accounts.some((a) => a.id === keep)) sel.value = keep;
}

async function showAccounts() {
  accounts = await listAccounts(db);
  render(
    accountsList,
    accounts.map((a) =>
      h(
        'tr',
        { class: 'account', 'data-id': a.id },
        h('td', { class: 'id' }, a.id),
        h('td', { class: 'owner' }, a.owner),
        h('td', { class: 'balance' }, a.balance),
      ),
    ),
  );
  for (const s of [fromSel, toSel, histSel]) options(s);
  if (!toSel.value || toSel.value === fromSel.value) {
    toSel.value = accounts.find((a) => a.id !== fromSel.value)?.id ?? '';
  }
}

async function showHistory() {
  const id = histSel.value;
  if (!id) return;
  if (pager?.account !== id) pager = new HistoryPager(db, id, PAGE);
  const rows = await pager.reload();
  render(
    history,
    rows.map((e) =>
      h(
        'li',
        { class: 'entry', 'data-at': e.at },
        h('span', { class: 'at' }, time(e.at)),
        h('span', { class: 'amount' }, money(e.amount)),
        h('span', { class: 'memo' }, e.memo),
      ),
    ),
  );
  histPage.textContent = `page ${pager.index + 1}`;
  newer.disabled = !pager.hasNewer;
  older.disabled = !pager.hasOlder;
  const month = monthOf(Date.now());
  const s = await statement(db, id, month);
  render(
    stmt,
    h('span', {}, `${id}, ${month}: `),
    h('span', { id: 'stmt-in' }, `in ${s.total_in}`),
    ', ',
    h('span', { id: 'stmt-out' }, `out ${s.total_out}`),
    ', ',
    h('span', { id: 'stmt-count' }, `${s.count} entries`),
  );
}

async function showAudit() {
  const rows: AuditRow[] = await listAudit(db, 20);
  render(
    audit,
    rows.map((r) => h('li', { class: 'audit' }, `${time(r.at)} ${r.from} → ${r.to}: ${r.amount}`)),
  );
}

// One refresh at a time; a change during one schedules one more.
let running: Promise<void> | undefined;
let again = false;
function refresh(): Promise<void> {
  if (running) {
    again = true;
    return running;
  }
  running = (async () => {
    do {
      again = false;
      try {
        await showAccounts();
        await showHistory();
        await showAudit();
      } catch (e) {
        console.warn(`refresh: ${message(e)}`);
      }
    } while (again);
  })().finally(() => {
    running = undefined;
  });
  return running;
}

newer.addEventListener('click', async () => {
  await pager?.newer();
  await showHistory();
});
older.addEventListener('click', async () => {
  await pager?.older();
  await showHistory();
});
histSel.addEventListener('change', () => void showHistory());

runStatements(db, (e) => console.warn(`statements: ${message(e)}`));
await db.on(entriesTopic(db), () => void refresh());
await db.on(statementsTopic(db), () => void refresh());
await runAuditView(db, {
  onRow: () => void refresh(),
  onError: (e) => console.warn(`audit view: ${message(e)}`),
});

render(
  root,
  h('h2', {}, 'Accounts'),
  h(
    'table',
    {},
    h(
      'thead',
      {},
      h('tr', {}, h('th', {}, 'account'), h('th', {}, 'owner'), h('th', {}, 'balance')),
    ),
    accountsList,
  ),
  h(
    'form',
    {
      id: 'open-form',
      onsubmit: async (e: Event) => {
        e.preventDefault();
        openError.textContent = '';
        try {
          await openAccount(db, {
            id: newId.value.trim(),
            owner: user,
            deposit: Number(deposit.value || 0),
          });
          newId.value = '';
          deposit.value = '';
          await refresh();
        } catch (err) {
          openError.textContent = message(err);
        }
      },
    },
    newId,
    deposit,
    h('button', { id: 'open-add', type: 'submit' }, 'Open account'),
  ),
  openError,
  h('h2', {}, 'Transfer'),
  h(
    'form',
    {
      id: 'tr-form',
      onsubmit: async (e: Event) => {
        e.preventDefault();
        trError.textContent = '';
        try {
          await transfer(db, {
            from: fromSel.value,
            to: toSel.value,
            amount: Number(amount.value),
            memo: memo.value,
          });
          await refresh();
        } catch (err) {
          trError.textContent = message(err);
        }
      },
    },
    fromSel,
    ' → ',
    toSel,
    amount,
    memo,
    h('button', { id: 'tr-send', type: 'submit' }, 'Send'),
  ),
  trError,
  h('h2', {}, 'History'),
  h('div', { class: 'row' }, histSel, newer, histPage, older),
  history,
  h('h3', {}, 'This month'),
  stmt,
  h('h2', {}, 'Audit (this device)'),
  audit,
);
await refresh();
ready(ctx, { refresh, accounts: () => accounts, pager: () => pager });
