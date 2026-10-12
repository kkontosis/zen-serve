# App examples: proof-of-concept targets for zen-db and the broker

Five small apps, specified here (milestone 4.5) and built in milestone 5 on the `Db` class of [`spec/zendb.md`](../spec/zendb.md). Between them they exercise every part of it:

| | Kanban | Chat | Booking | Checkout | Ledger |
|---|---|---|---|---|---|
| rows, transactions | ✓ | ✓ | ✓ | ✓ | ✓ |
| private index (order, paging) | ✓ | ✓ | ✓ | | ✓ |
| unique index | | | ✓ | ✓ | |
| fast index | | ✓ | | | |
| files in a transaction | ✓ | ✓ | | | |
| change events | ✓ | | | | ✓ |
| `on` (live, durable) | ✓ | ✓ | | | ✓ |
| work queue / consumer | ✓ | | ✓ | ✓ | ✓ |
| request/reply | | ✓ | ✓ | ✓ | |
| delayed messages | | | ✓ | ✓ | |
| saga | | | | ✓ | |
| ephemeral | ✓ | ✓ | | | |

**Notation:**
* Tables are written `name(pk; fields) [indexes]`.
* Topics are paths.
* A transaction is listed with what its commit carries (zendb.md §7.3).
* All apps run in one fs whose members hold `read`, `write` and the topic rights listed under each app (formats.md §9.1).

---

## 1. Kanban board with attachments

A family or small-team board: columns, cards and files attached to cards. Several people edit at once, and changes appear live.

**Schema** (database `kanban`):
* `boards(id; name)`
* `columns(id; board, name, pos)` [private `(board, pos)`]
* `cards(id; board, column, pos, title, text, attachments: [{node, name, size}])` [private `(column, pos)`]; `changes: {image: "full"}`

**Files:** one fs tree per board (fs.md), with a directory per card holding its attachments. The card row stores the attachment node ids.

**Topics:**
* the change topic of `cards`
* `("board", board_id)` for ephemeral cursors ("Alice is dragging…")

**Transactions:**

| Action | Commit |
|---|---|
| move a card | reads `cards` TableRecord, the card, the `(column, pos)` root; writes the card and the rewritten index path; appends a change event |
| reorder | positions are fractional keys (a string between its neighbours), so a move rewrites one card, never the column |
| attach a file | earlier commits upload the chunks (`chunks`, fs.md §6 grace period); then one transaction: card row with the new attachment + `crdt_ops` `move` (create the file node) + `write` (its version) + change event |
| delete a card | card row deleted, its attachment directory moved to TRASH (a `move`), change event |

**Live view:**
* `cards.on(...)` on the change topic, from the board's last seen offset.
* Concurrent moves of the same card conflict. One retries and lands after the other, so there is no duplicated card.

**What it shows:**
* rows and files atomically in one commit (zendb.md §7.6, §16.2)
* history-independent ordered indexes
* change events driving the UI

**Grants:** fs `read`/`write`; `read`/`append` on the change topic and `("board")`.

**Milestone 5 demo:**
1. Two browsers on one board.
2. Drag and attach in one, and watch the other update.
3. Kill a tab mid-upload: no card refers to a missing file. Unreferenced chunks are collected after the grace period.

## 2. Family chat

Rooms, messages, read receipts, typing indicators, and a "who's online" query.

**Schema** (database `chat`):
* `rooms(id; name, members)`
* `receipts([room, device]; offset)`
* `blobs(id; room, node)` for shared photos, which are fs files

**Topics:**
* `("chat", room)` for messages, durable
* `("chat", room, "typing")` for typing, ephemeral
* `("presence")` for presence requests

**Flows:**
* **Send:** `emit(("chat", room), "msg", {text, reply_to?})`, alone, or in a transaction with a photo's file op and its `blobs` row.
* **Read:** each device `on(("chat", room), …, {after})` from its `receipts` offset. It shows history, then live messages.
* **Mark as read:** a transaction updates the device's `receipts` row. A durable `on` with `cursor` does the same automatically.
* **Typing:** `publishEphemeral` / `onEphemeral`: nothing stored, rate-limited.
* **Who's online:** a scatter-gather `request(("presence"), "ping", {}, {timeoutMs: 1500, reply: "ephemeral"})`. Every running instance answers from its `on` handler.
* **Search:** a fast index on `blobs.room` lists a room's photos. It leaks the photo count per room, which is acceptable here and documented as such.

**What it shows:**
* `on` with resume across reconnects and restarts
* ephemeral messages next to durable ones
* scatter-gather request/reply

**Grants:** fs `read`/`write`; `read`/`append` on `("chat")` and `("presence")`; `read`/`append` on `("zen", "inbox")`.

**Milestone 5 demo:**
1. Three devices.
2. One goes offline, the others send messages, it comes back: it sees every message exactly once, in order.
3. Typing indicators never reach the log.

## 3. Slot booking

A shared calendar of bookable slots (a sports court, a car): one booking per slot, a waitlist, and reminders.

**Schema** (database `booking`):
* `slots(id; resource, start, end)` [private `(resource, start)`]
* `bookings(id; slot, who, created)` [unique `slot`, kind `none`]
* `waitlist(id; slot, who, created)` [private `(slot, created)`]

**Topics:**
* `("booking", "released")`, keyed by slot
* `("booking", "notify")` for notifications

**Transactions:**

| Action | Commit |
|---|---|
| book | insert into `bookings`: the unique entry `("u", by_slot, slot)` must be absent (read conflict or `expect` null); schedule a reminder `at = start − 1h` |
| two people book the same slot | both read the unique entry absent; the first commit wins; the second gets `conflict`, retries, finds the entry, and fails with `unique_violation` |
| cancel | delete the booking (and its unique entry), `cancel(reminder)`, emit `released` keyed by the slot |
| waitlist promotion | a `per_key` consumer of `released` (one key per slot, so promotions of one slot are sequential): reads the first waitlist row by `(slot, created)`, inserts its booking, deletes the waitlist row, emits `notify`, all with the consume step |

**What it shows:**
* uniqueness without leaking more than existence
* a `per_key` work queue
* delayed messages and their cancellation
* the scheduler leader

**Grants:** fs `read`/`write`; `read`/`append`/`consume` on `("booking")` and `("zen", "db", "booking")`.

**Milestone 5 demo:**
1. Twenty concurrent bookings of one slot: exactly one succeeds.
2. Cancel it: the head of the waitlist is promoted exactly once.
3. Kill the scheduler leader before a reminder is due: another instance takes over and sends the reminder late, once.

## 4. Checkout saga

An order flows through payment, stock and shipping, which are separate services, maybe on separate devices (Node workers). A failure compensates the steps already done.

**Schema** (database `shop`):
* `orders(id; customer, items, total, status)`; `changes: {image: "keys"}`
* `payments(id; order, amount, status, provider_ref)` [unique `order`]
* `stock(sku; available)`
* `reservations(id; order, sku, n)` [unique `(order, sku)`]
* `shipments(id; order, status)`

**Saga `checkout`:**

| Step | Command topic | Compensation | Timeout |
|---|---|---|---|
| `reserve` | `("shop", "stock", "reserve")` | `("shop", "stock", "release")` | 10 s |
| `pay` | `("shop", "pay")` | `("shop", "refund")` | 60 s |
| `ship` | `("shop", "ship")` | – | 1 day |

**Flows:**
* **Place an order:** one transaction inserts the order (`status: "placed"`) and calls `checkout.start({order})`. It inserts the `$sagas` row, emits the `reserve` command and schedules its timeout.
* **Stock worker:** `serve` on `reserve`, `per_key` by sku. One transaction checks `stock.available`, decrements it, inserts the reservation and replies `ok`, or replies `fail` when stock is short.
* **Payment worker** (the external effect, zendb.md §12.6):
  1. `serve` on `pay` emits a command to `("shop", "effects", "charge")` in its transaction.
  2. An effects worker calls the payment provider with the idempotency key = the command's `id`.
  3. It records `payments` and replies `ok`/`fail` to the saga in one transaction with its consume step.
* **Orchestrator:** advances the saga per zendb.md §12.5, and sets the order `status` in the same transactions. On `fail` or timeout it compensates: `refund` (if paid), then `release`.

**What it shows:**
* sagas with timeouts and compensations
* exactly-once transitions
* at-least-once external calls with idempotency keys
* a consumer per service with workers on different devices

**Grants:** fs `read`/`write`; `read`/`append`/`consume` on `("shop")`, `("zen", "saga", "shop")`, `("zen", "db", "shop")` and `("zen", "inbox")`.

**Milestone 5 demo:**
1. Run one order through.
2. A payment that fails: stock is released.
3. A payment worker killed after the provider call: the retry reuses the key, so there is one charge.
4. Run 100 orders while killing workers at random: every saga ends `done` or `failed`; stock and payments balance.

## 5. Credits ledger

Family pocket money or team credits: balances that must never go negative, a full audit trail, and a monthly statement view.

**Schema** (database `ledger`):
* `accounts(id; owner, balance)`
* `entries(id; account, amount, at, memo)` [private `(account, at desc)`]; `changes: {image: "full"}`
* `statements([account, month]; total_in, total_out)`

**Topics:**
* the change topic of `entries`
* `("ledger", "audit")`

**Transactions:**

| Action | Commit |
|---|---|
| transfer | short transaction: read both accounts; fail if the source would go negative; update both balances; insert two `entries`; emit `audit` `{transfer, from, to, amount}`. All in one commit, so balances, entries and audit never disagree (the outbox) |
| two transfers from one account at once | both read the balance; the second commit conflicts, retries on the new balance, and is refused if it would overdraw |
| statement projection | a `sequential` group (`statements`) on the change topic: each change event updates the month's `statements` row with the consume step, so totals count each entry exactly once |
| audit view | a durable `on(("ledger", "audit"), …, {cursor: "audit-view"})` per device that keeps a local read model |

**What it shows:**
* invariants under concurrency
* the transactional outbox
* a projection (read model) from change events, exactly once
* paged history through a descending private index

**Grants:** fs `read`/`write`; `read`/`append`/`consume` on `("ledger")` and `("zen", "db", "ledger")`.

**Milestone 5 demo:**
1. Many concurrent random transfers.
2. Check:
   * the sum of balances is constant
   * no balance is negative
   * the statements equal the sums of the entries
   * the audit log has one event per transfer
3. Restart the projection mid-stream: the totals are unchanged.

## CRDT tables in the examples

The server side of CRDT tables comes in milestone 5 (spec/zendb.md §19, `TD-CRDT-ROWS-SERVER`). With it, two of the examples gain an offline-first variant, demonstrated in milestone 5 as well:

* **Kanban.**
  * Cards become a CRDT table: `title` and `text` are `lww`, `labels` a `set`, `votes` a `counter`.
  * Cards edited offline merge field by field on reconnect, and the board's `(column, pos)` index is kept by the indexer (§19.6).
  * Moving a card between columns stays last-writer-wins, so two concurrent moves end with one winner, never a duplicate.
* **Chat.**
  * Reactions are a `counter` per emoji on a CRDT `reactions(message_id)` row, and room membership a `set`.
  * Both work offline. Neither needs an invariant, so neither needs a transaction.

Slot booking, checkout and the ledger stay transactional: their invariants (one booking per slot, no negative balance) are exactly what CRDT tables give up.


## As built (milestone 5)

The five apps are in [`apps/`](../apps/) (layout, build and tests: [`apps/README.md`](../apps/README.md)). Each runs in Chromium from `apps/dist/<name>/` served by zen-serve's `unencrypted_dir`, signs in with a password and unlocks the fs with a passphrase. Its tests run with only the grants above (`apps/<name>/test/grants.ts`): vitest scenarios for the invariants and faults, Playwright specs for the demos in two or three browsers.

* **Kanban** (`apps/kanban`):
  * as specified, plus the Loro card text (`card/<id>`, `@zen/loro`); the row's `text` is a short description
  * the change topic of `cards` is the table's default, under `("zen", "db", "kanban")`
  * each card's directory is made when the card is, in the same commit
  * the offline variant is a second table, `crdt_cards`, in the same database (`?crdt=1`): `column` and `pos` are `lww` too, patched together so a move wins or loses whole; it has no attachments, since a `lww` list would lose concurrent attaches
  * a killed upload is a Node worker SIGKILLed mid-upload; its chunks are gone after the grace period
* **Chat** (`apps/chat`):
  * `rooms` is a CRDT table (name `lww`, `members` a set); `reactions` has four counter fields (like, love, laugh, party), since counter fields are declared
  * a message body also carries the sender's display name and an optional photo (`blobs` id)
  * presence: the request is an event on `("presence")`; each instance answers with an ephemeral reply (`db.reply(m, body, {ephemeral: true})`)
  * Chromium's offline mode keeps an open WebSocket, so the page stops reading on `offline` and resumes from the last offset shown on `online`
  * no private index (§2 lists none, though the table above ticks one)
  * found TD-EPHEMERAL-SAME-DEVICE: two sessions of one device can drop each other's ephemeral messages
* **Booking** (`apps/booking`):
  * as specified; reminders and promotions are message types `reminder` and `promoted` on `("booking", "notify")`, keyed by slot
  * a promotion consumes the event without booking when the slot was booked again meanwhile
  * the reminder's id is kept in the booking row, which `cancel` cancels
* **Checkout** (`apps/checkout`):
  * the services run as Node workers (`apps/checkout/workers`), one member each: stock, payment, effects, shipping, a dedicated orchestrator, and a mock payment provider charging once per idempotency key
  * `pay` is a `consume` that forwards the command (with its reply route) to `("shop", "effects", "charge")`; the effects worker records the payment and replies to the saga in one transaction. Refunds go through the effects worker too.
  * stock commands are `per_key` by saga id (the saga keys every command by it); per-sku consistency comes from the transactions
  * a participant checks that its command is still current and its timeout pending before acting; a charge made after its timeout is refunded (`voided`)
  * the order's status is copied from the saga's state by a second group (`order-status`), one commit behind (TD-SAGA-HOOKS)
  * consumers poll with `waitMs: 2000` (TD-CONSUME-CLAIM-WAKE); `$sched` is the contention point under load (TD-SCHED-HOTSPOT)
* **Ledger** (`apps/ledger`):
  * as specified, plus `accounts.at` (entry times strictly increase per account), `entries.transfer`, `statements.count`, an index on `accounts.owner`, and `statements` change events so other pages refresh
  * the audit view keeps its read model in an `audit_view` table per device and reads the log from its start (TD-DB-ON-FROM-START)
  * an overdraft is the transaction's result (its reads are still checked), then thrown as `LedgerError`; `transfer` retries with jitter when every attempt conflicts
