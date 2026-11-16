# Git over zen-serve

Git repositories kept in an encrypted fs. Packs are stored as immutable files, refs as rows of a zen-db database, and a push is one transaction: the pack's files, a compare-and-set of the refs, and an event for CI workers. The server stores and relays all of it blind (§10).

Stock git reaches a repo in one of three ways (§1). All three share one core (§6): a local bare **mirror** synced from zen, served by git's own `upload-pack` and `receive-pack`, and a `pre-receive` hook that does the push's transaction.

Built in milestone 10 (docs/MILESTONE-10.md). The design rests on step 10.2's spike with git 2.43, whose findings and numbers are in that file's log.

## 1. Three ways in

| Way | URL | Runs on | Plaintext on |
|---|---|---|---|
| **`git-remote-zen`**, the remote helper (§7) | `zen://<host>/<fs>/<repo>` | the member's machine, run by git | that machine: the clone and the mirror |
| **The local server** `zen-git serve` (§8.1) | `ssh://localhost:<port>/<fs>/<repo>`, `http://127.0.0.1:<port>/<fs>/<repo>` | the member's machine, one long-running process | that machine: the mirrors |
| **The relayed service** (§8.2) | `ssh://git@<host>:<port>/<fs>/<repo>` | a handler machine of a member (svc.md) | the handler machine, for every repo it serves |

* `<host>` is the zen-serve server. `<fs>` is the fs id in decimal. `<repo>` is a repo name (§2.1); a trailing `.git` in a URL is ignored.
* **The helper's schemes:** `zen://<host>[:<port>]/<fs>/<repo>` reaches the server over HTTPS. `zen+http://` reaches it over plain HTTP, for a server on localhost and for tests. Nothing else is accepted: no user, password, query or fragment. The sign-in origin (auth.md §5.1) is the URL's `https://` or `http://` origin.
* The helper and the local server sign in through the vault as the member's device (CLIENT.md, `credentialVault`), without a prompt. The vault is a sealed local store (`sealedStore` with the device layer) in the directory `$ZEN_GIT_VAULT`, or else `$XDG_CONFIG_HOME/zen-git/vault` (`~/.config` by default). `zen-git login` puts the device in it once, from a device key file. A helper whose vault holds no device fails with a message naming `zen-git login`, and never prompts: git may run it with no terminal.
* **Unlocking:** the helper unlocks the fs with the vault's device, so the fs needs a device keyslot for it (formats.md §6; `fs.addDevice`, by an admin). Integrity (formats.md §14) is attached first, its pins kept in the vault, and the vault's device signs the checkpoints.

## 2. A repo

### 2.1 Names

A repo name is 1 to 100 bytes of `[A-Za-z0-9._-]`, not starting with `.` and not ending in `.git`. Names are unique within the fs (the `repos` table's key, §3).

### 2.2 Files

* Each repo has a **tree** of its own (fs.md §1), whose id is in its `repos` row. So pushes to different repos don't serialize on one tree's header (fs.md §7).
* The tree holds only packs, under git's own layout and names:
  ```
  /objects/pack/pack-<hash>.pack
  /objects/pack/pack-<hash>.idx
  ```
  `<hash>` is git's pack hash (the pack's trailing checksum, in hex). Names are sealed in node metadata (fs.md §1, formats.md §11.2), so the server never sees them.
* A pack is **written once**, by the transaction that adds its row (§4 or §9), and never changed. It is deleted only by a repack's gc (§9).
* Every pack is **self-contained**: never thin, so each of its deltas has its base in the same pack. Its objects may refer to objects in older packs (a commit's parents, say).
* The `.idx` is git's index of the pack. Readers don't trust it: an import rebuilds it (§5). It is stored for readers that read objects from a pack directly.
* The fs's chunking policy applies (fs.md §4.1). Content-defined chunks, the default, save about a fifth of the bytes between two full repacks of one repo, and little between small packs and their repack (the spike). So repacks roll small packs up instead of rewriting everything (§9).

### 2.3 Object format

A repo is `sha1` or `sha256` (git's `--object-format`), fixed when it is created by its first push. Every mirror of it is created with that format. Oids are stored as raw bytes: 20 or 32.

## 3. The database

Every repo of an fs is in one zen-db database of that fs, `ns = "git"` (zendb.md §2.1), `authenticated` by default (zendb.md §14), which needs integrity attached to the fs (formats.md §14); without it, `basic`. Every table is `pad: true` (zendb.md §4.3).

| Table | Primary key | Fields |
|---|---|---|
| `repos` | `name` | `format` (`"sha1"` or `"sha256"`), `head` (a ref name, HEAD's target), `tree` (16 bytes), `next_seq` (u64), `created_ms`, `creator` (device id) |
| `refs` | `[repo, ref]` | `oid` (bytes), `device` (the pusher's device id), `time_ms`, `seq` (the push's, below) |
| `packs` | `[repo, hash]` | `seq` (u64), `size` (bytes), `objects` (count), `nodes` (`[pack, idx]` node ids), `state` (`"live"`, `"replaced"`, or `"pending"`: a repack's pack whose files are written and which isn't live yet, §9), `replaced_ms`, `by` (the hash of the pack that replaced it), `of` (a repack's pack only: the hashes it rolled up, §9) |

* `refs` has a private index `by_repo` on `repo`, and `packs` a private index `live` on `[repo, state, seq]` (zendb.md §5.4). So one repo's refs are read without reading the others', and its live packs are one range read in `seq` order, without its replaced ones.
* **The packs table is the pack manifest:** a repo with thousands of pushes lists its packs with that one range read, and repacks (§9) keep the live ones few. There is no other listing.
* `seq` orders a repo's packs by push. Each push takes `next_seq` from its `repos` row and increments it (§4), so a pack's objects refer only to objects in packs of a lower `seq`, or in the same pack. A repack's pack takes the highest `seq` it rolls up (§9), which keeps that rule.
* HEAD is `repos.head`, set by the first push to the first `refs/heads/` ref it pushes (as a stock server's first push does). No other symbolic refs are kept.
* **Why `authenticated`:** git's objects check themselves by their oids, so the refs are what a server could serve stale. In a `basic` database it could serve an older ref, or leave a ref out of a listing (zendb.md §14.1); an `authenticated` database detects both, up to the checkpoint window (zendb.md §14.7). A `basic` database is allowed, and loses that.

## 4. A push

A push is cut as one pack (or none) and applied in one transaction:

1. **The pack.** `git pack-objects --revs --stdout` of the pushed tips, excluding (`^`) every tip zen had when the mirror was synced, never `--thin`. Then `git index-pack --strict` in the mirror, which checks every object and writes the `.idx`. A pack with no objects (a push of known objects, or of deletes only) is dropped.
2. **The upload.** The pack's and the index's chunks go up under an upload session (api.md §12.1), before the transaction, so a large pack's transaction carries only its `write`s (zendb.md §7.6). An upload interrupted here leaves nothing visible, and the session keeps what it uploaded for a resume.
3. **The transaction** (zendb.md §7.1), whose function zen-db runs again on a `conflict` (item 4):
   1. Read the `repos` row (a new repo has none: it is created here) and every `refs` row the push updates.
   2. Each ref's current oid must equal the push's old value (the mirror's, which `receive-pack` gives the hook): absent for a new ref. If one differs, another push landed since the sync, and the hook refuses every ref it was given (`fetch first`).
   3. Write the pack's two files into the repo's tree, and put its `packs` row with `seq = next_seq`. Put the `repos` row with `next_seq + 1`.
      * **A known pack is skipped:** when the repo has a `packs` row of that hash, `live` or `replaced`, no files and no row are written (the refs still move). A replaced pack's objects are in a live pack, so the push adds nothing, and no two nodes get one name.
      * **A `pending` row of that hash** (a repack's files of the same objects, §9) is made `live` with this push's `seq`: its files are already written.
   4. Put each updated `refs` row, or delete it for a deleted ref.
   5. Emit one event to the topic `("git", repo, "pushes")` (zendb.md §11.1):
      ```
      {1: repo, 2: device, 3: [{1: ref, 2: old oid | null, 3: new oid | null}], 4: pack hash | null, 5: seq}
      ```
      **And a repack request:** when the push adds a pack and the repo then has more than 16 live packs (a range read of `live`, limited to 17 rows), it also emits `repack` `{1: repo}` to the fs's topic `("git", "repack")` (§9).
   6. Commit.
4. **A conflict** at commit (a concurrent push read or wrote the same rows):
   * zen-db runs the function again (up to its `attempts`), which re-reads. When every updated ref still has its old value, only the `repos` row moved, as when two pushes update different branches: the push goes on, with the pack reused and its `seq` taken again.
   * Otherwise step 3.2 refuses it (`fetch first`), and git reports it as it reports a non-fast-forward.
* **Exactly one of two racing pushes to one ref wins** (spike: two clones pushing at once).
* **Fast-forward, `--force`, `--force-with-lease` and `--atomic`** are checked by `receive-pack` against the mirror's refs, which equal zen's refs read in step 3.1. So they are git's own checks. A push that is not `--atomic` but refused for one ref is applied for the others, as git does.
* **The window after the hook.** The hook commits before `receive-pack` updates the mirror's refs. If that update fails (a crash, a full disk), zen has the push but git reports `failed to update ref`, and the next sync (§5) brings the mirror to zen's refs. So a push reported as failed may have happened, as on a stock server whose ref update fails after its hook. To keep ref locks out of that window, a mirror is held exclusively for the whole `receive-pack` (§6). A `reference-transaction` hook would hold git's locks, but it runs once per ref unless the push is `--atomic`, which would split one push into several transactions (the spike).

## 5. A fetch: syncing a mirror

Before `upload-pack` or `receive-pack` runs, the mirror is synced:

1. **One read transaction** reads the `repos` row, the repo's `refs` rows, and its live `packs` rows. That is one consistent state.
2. **The packs the mirror lacks** are downloaded in `seq` order. Each is imported with `git index-pack --strict` inside the mirror, which rebuilds its `.idx` and checks every object and its links to the packs already imported. Hence the order: `--strict` refuses a pack whose objects refer to objects the repository doesn't have yet. The mirror records the imported packs' hashes, so it can repack its own files (§6) and still know what it holds.
   * **A repack's pack is covered** when every hash in its `of` is recorded as imported: the mirror already has its objects, so it is recorded as imported too, without a download. So a mirror that kept up with the pushes never downloads a roll-up. One that missed a whole roll-up (idle through two repacks of the same packs) downloads the newer one, which is only larger.
3. **The mirror's refs are set to zen's:** refs absent from zen are deleted, and the others set to zen's oids (`git update-ref --stdin`). HEAD is set to `repos.head`.
4. **Then git's service runs** on the mirror.

* **A pack that fails to import** (a bad object, a missing base) stops the sync with an error naming the pack. Nothing is served from a mirror that failed to sync.
* **A pack that has gone:** a state read before a repack lists packs that its gc may since have deleted (§9). A download that finds its node gone (`not_found`) makes the sync read the state again (step 1) and start over, once; the new state lists the repack's pack instead. A second failure is an error.
* **Shallow and partial clones** (`--depth`, `--filter`) work, since `upload-pack` serves them from the mirror. The mirror itself is always complete.

## 6. The mirror

* A bare repository with the repo's object format, kept on the machine that serves git:
  * the helper's, per user: `$XDG_CACHE_HOME/zen-git/<host>/<fs>/<repo>.git` (`~/.cache` by default; `<host>` is the URL's `host[:port]`, so two servers' fs ids don't meet), so every clone of a repo on the machine shares one
  * the local server's and the handler's: the same layout, under their own data directory
* Its config: `uploadpack.allowFilter true`, `uploadpack.allowAnySHA1InWant true`, `receive.unpackLimit 1`, and the `pre-receive` hook of §4.
* **Locking:** a sync and a `receive-pack` hold the mirror exclusively; `upload-pack` takes no lock. So no two pushes on one machine meet in one mirror, and none of git's ref locks is contended.
  * The lock is a directory next to the mirror, `<repo>.git.lock/`, made with `mkdir` (atomic), holding a file `pid` with the holder's process id. A process that finds it waits and tries again. It takes the lock over when that pid is no longer running (a crashed holder), by removing the directory and making it again. Node has no portable `flock`, and `mkdir` works on every local file system.
  * `upload-pack` needs no lock: git's readers tolerate refs being updated, packs being added, and `git repack -a -d` running while they read (they rescan the packs). The sync before it holds the lock, and releases it before `upload-pack` starts.
* **It repacks itself** when it holds more than 16 packs (`git repack -a -d`, local only). That kept 4 MB of the 28 MB that 585 small packs took (the spike).
* **Git's environment:** git sets `GIT_DIR` (and may set `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_PREFIX`, `GIT_COMMON_DIR`, `GIT_NAMESPACE`) for the helper. Those are removed for every git command run on the mirror, or `GIT_DIR` would override `-C`.
* **Plaintext:** the mirror holds the repo's full plaintext history, as a clone does. The docs say so, and recommend disk encryption.

## 7. `git-remote-zen`

The remote helper (gitremote-helpers(7)) for `zen://` URLs. Git runs `git-remote-<scheme>`, so the same program is installed under two names: `git-remote-zen` and `git-remote-zen+http`. It speaks only:

* **`capabilities`** → `connect`, `option`
* **`option <name> <value>`** → `ok` for `verbosity` and `progress`, `unsupported` for the rest (git then keeps its defaults)
* **`connect <service>`**, for `git-upload-pack` and `git-receive-pack`: it syncs the mirror (§5), answers an empty line, and then runs the service on the mirror with its own stdin and stdout, which git talks to directly.
  * **Protocol v2:** git gives a `connect` helper no `GIT_PROTOCOL`, so the helper starts `git-upload-pack` with `GIT_PROTOCOL=version=2`, unless git's `protocol.version` is 0 or 1. Git's client detects the v2 advertisement. Without it, `push.negotiate` fails with a warning (the spike).
  * The helper reads its commands one byte at a time, so nothing after the `connect` line is consumed before the service takes over.
* The `receive-pack` service's `pre-receive` hook does §4's transaction, signed in as the helper's device.
  * The hook is a process of its own, which `receive-pack` starts, so it signs in again from the same vault. The vault keeps the helper's session, so that sign-in resumes it rather than signing a new challenge (STATS gives the cost).
  * The helper passes the hook what it needs in its environment: the repo's URL, and the command that runs `zen-git hook`. The hook script refuses a push without them, so a push made straight into the mirror's directory with stock git (not through the helper) is refused and changes nothing.
  * The hook cuts the pack (§4 step 1) with git's quarantine variables in place (`GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`), since the pushed objects are in the quarantine until it succeeds. It excludes every ref the mirror has, which are zen's refs as of the sync.

## 8. The servers

### 8.1 The local server

One process (`zen-git serve`) serves every repo the device can open, to stock git, IDEs and containers on the same machine. It signs in through the vault as the device.

* **SSH,** through the SSH engine (svc.md §7.2), with its host keys:
  * exec commands `git-upload-pack '<path>'` and `git-receive-pack '<path>'` only, with `<path>` = `/<fs>/<repo>` (a trailing `.git` ignored); anything else is refused
  * user keys from an allowed-keys file the server keeps (`authorized_keys` lines: `ssh-ed25519`, `rsa-sha2-*` and `ssh-mldsa44-ed25519`)
  * protocol v2: git sends `GIT_PROTOCOL` in an `env` request (`ssh -o SendEnv=GIT_PROTOCOL`). That one variable is accepted and passed to the service; without it, v0 (the spike)
* **Smart HTTP,** through `git http-backend` (CGI) on the mirrors:
  * bound to `127.0.0.1` (and `::1`); another address only by explicit configuration
  * `Host` must be `localhost`, `127.0.0.1` or `[::1]`, with or without the port, or the answer is 421 (DNS rebinding)
  * HTTP Basic auth, any user name, the password a token from `zen-git token`: 32 random bytes in base64url, kept hashed by the server, and stored by git's credential helper on the client. A wrong token is 401
  * the `Git-Protocol` header is passed to the backend, so protocol v2 works
* Both serve the same mirrors, with §6's locking.

### 8.2 The relayed service

A relayed service (svc.md) whose handler is a `zen-git serve` on a member's machine: stock git anywhere reaches `ssh://git@<host>:<port>/<fs>/<repo>`, with nothing installed. The zen-serve server stays a blind relay, and handlers take over from each other (svc.md §6).

* **Keys to members:** the `ssh_keys` table of the `git` database, `fingerprint` → `{member: user id, key: the public key line, sig}`. `sig` is the member's hybrid signature (formats.md §7.3) with their user identity, purpose `zen/v1/sig/git-ssh-key`, of `u32(fs) ‖ lp(fingerprint) ‖ lp(key)`. So a member can add only their own keys, and a handler accepts a key only with a valid signature by a member of the fs's signed ACL (formats.md §9).
* **Rights:** the key's member's fs grants (§10), from the signed ACL. The handler pushes as its own device, and the push's `refs.device` and event name the handler. The event also carries the member: field `6: member user id`.
* **The handler sees the plaintext** of every repo it serves, and the keys of the members who use it. Encryption ends at the handler machine.

## 9. Repack and gc

Built in 10.4 (`zen-git worker`, `zen-git repack`).

* **When:** a repo with more than 16 live packs. A push that passes that count emits `repack` `{1: repo}` to `("git", "repack")` (§4 step 3.5), **one topic per fs** for every repo in it. The repack job is the consumer of the `sequential` group `"repack"` there (zendb.md §11.3), so one runs at a time per fs, under one lease, and it serves repos created after it started. A job runs in any client of a member with fs `write` (`zen-git worker`, one per fs, on an always-on machine). Without one, packs pile up: fresh clones slow down, and the mirrors still repack themselves (§6).
  * A `repack` message for a repo that has 16 live packs or fewer (an earlier message's repack covered it) is acknowledged with nothing done.
* **Which packs: a geometric roll-up** (factor 2, as `git repack --geometric=2`) of the newest packs. In `seq` order, take the newest two at least, and the shortest such run that leaves every older pack at least twice the bytes of all the packs after it together. Rolling up only the newest keeps §3's order. The large old packs are rewritten only when the small ones add up to their size, so a repack's cost stays near the size of what was pushed since the last one.
* **The new pack** is built from the rolled-up packs alone, so the job needs no mirror and downloads only those packs:
  1. A temporary bare repository with the repo's object format. Each rolled-up pack is downloaded into it and indexed with `git index-pack` **without `--strict`**, since its objects may refer to older packs, which aren't there. Every pack is self-contained (§2.2), so each delta's base is present.
  2. `git cat-file --batch-all-objects --batch-check` lists every object, which `git pack-objects --stdout` packs, never thin. Then `git index-pack` checks it and writes its `.idx`.
  3. Its object count must equal the number of distinct objects of the rolled-up packs, or the job stops with an error and changes nothing.
  4. Its two files go up under an upload session (§4 step 2).
* **Short commits** (refs don't change). Every writer of the authenticated `git` database conflicts with every other on its NsRoot (zendb.md §14.5), and every commit with tree operations serializes at the tree's header (fs.md §7). So a repack is a few short commits, each of which lands between pushes, and each conflict retries only that commit, never the build. The bench in docs/MILESTONE-10.md (step 10.4) found what one long commit does under a steady stream of pushes: it lost every time, and the live packs grew to 1,800.
  1. **The files:** if the repo has no row of the new hash, write the new pack's files and put its row as `pending` (`seq` = the highest rolled-up `seq`, `of` = the rolled-up hashes), and schedule a `gc` message for it after the grace (zendb.md §12.4, `tx.schedule`). A `pending` row there already means another job is repacking: the job is refused (`the packs changed`).
  2. **Live:** one range read of the repo's live packs. Every rolled-up pack must still be among them, or the job is refused (`the packs changed`), with nothing changed but the `pending` row, which its gc removes. Then the new row is made `live`.
  3. **Replaced:** the rolled-up rows still `live` are marked `replaced` (with `replaced_ms` and `by`, the new hash), 16 per commit. Each of these commits schedules a `gc` message to `("git", "repack")` for its rows, due after the grace (24 h by default): `{1: repo, 2: [hash]}`.
  * Between steps 2 and 3, the new pack and some of the packs it rolled up are live together: the same objects twice, which a sync imports and git accepts. A job that stops there leaves them so, and the next repack rolls them up again.
  * Pushes committed meanwhile change none of the rows these commits touch.
* **Gc:** at a `gc` message the job takes the listed rows that are still `replaced` or `pending`, moves their files' nodes to the tree's `TRASH` (fs.md §6), which the sweeper purges after its horizon, and deletes those rows, 16 per commit. A `live` row is left alone, so the gc that step 1 schedules does nothing once step 2 landed. Until then, a fetch that listed the replaced packs before the repack still finds them; after it, such a fetch re-reads the state (§5).
* **A job that stops** (a crash, a kill) after its upload changes nothing: its upload session's chunks are released when the session ends (api.md §12.1). One that stops between steps 1 and 2 leaves a `pending` row, which no reader lists and its gc removes. Its message, not acknowledged, is redelivered to the next job once the lease lapses.
* **The lease:** the client renews a group's lease only between deliveries, so the job renews it every 3 s while it works; a large roll-up outlasts the 10 s lease.
* **The scheduler** (zendb.md §12.4) delivers the `gc` message. It runs in the job's database, and leads through the topic `("zen", "db", "git", "sched")`, so the job's member needs rights there (§10).
* In the spike, 585 small packs rolled up into one in 0.94 s (`git repack --geometric=2`).

## 10. Rights

* **The fs grants** (formats.md §9.1): `read` to fetch and to publish (§13), `write` to push and repack. A grant covers the whole fs, so a repo with its own readers or writers lives in an fs of its own.
* **The topic:** a pusher needs `append` on `("git", repo, "pushes")`, or the push's transaction fails (403) and nothing is pushed. CI consumers need `read` and `consume` there.
  * A push can't grant these: only an admin changes the ACL (formats.md §9). So an admin grants them once per fs, with `zen-git setup <host>/<fs>`, on the topic prefix `("git")`, which covers every repo's topic:
    * `append` to every member with fs `write`
    * `read` and `consume` to every member with fs `read`
    * and `read` and `consume` on `("zen", "db", "git", "sched")`, the scheduler's lease topic (§9), to every member with fs `write`, so any writer can run the repack job
  * The repack topic `("git", "repack")` is under the prefix, so a writer appends there (§4 step 3.5) and a job consumes there with the grants above.
  * `setup` adds only the grants missing, so running it again (after adding members) is safe. A member added later has no topic grants until it runs again.
* **Branch protection, force-push rules and protected tags are cooperative.** The local server and handlers enforce what the repo's rules say for pushes through them. A member with fs `write` can still write any ref with a client of their own: the server can't read refs, so it can't check them (TECH_DEBT TD-GIT-REF-POLICY).

## 11. What the server sees

* **Pushes:** when a device pushes to the fs, the sizes of each pack's chunks and pages and its commit, the number of rows changed in padded buckets, and the event. It learns that two pushes touched the same rows when they conflict.
* **Fetches:** which pack chunks a device fetches and when, so how much was new to it.
* **Repacks:** a run of rows marked replaced and a larger pack written, the `repack` topic's events and their times, a scheduled row, then files and rows deleted a day later.
* **The public mirror (§13):** nothing, unless it is published through the server's `/unencrypted`; then its contents are public.
* **Which repo, unnamed:** each repo is a tree of its own (§2.2) with a topic of its own, so the server sees how many repos an fs has, and which one each push, fetch and repack touches, without their names.
* **Never:** repo names, ref names, oids, file names (all sealed), or contents.
* **The relayed service:** what svc.md §12 lists: the TCP connection's endpoints, timing and byte counts, and which handler serves it.

## 12. What isn't supported

From the spike's feature matrix (docs/MILESTONE-10.md, step 10.2): clone, fetch (also `--depth` and `--filter`), push (also `--force`, `--force-with-lease`, `--atomic`), deletes, light and annotated tags, HEAD, and sha1 and sha256 repos all work. Not supported (TECH_DEBT TD-GIT-SCOPE):

* **push options** (`git push -o`): `receive.advertisePushOptions` is off, so git refuses them ("the receiving end does not support push options")
* **signed pushes** (`git push --signed`): no `receive.certNonceSeed`, so git refuses them ("the receiving end does not support --signed push"); `--signed=if-asked` pushes unsigned
* **Git LFS**, and **submodules** as one unit (each submodule is a repo of its own, with its own URL)
* **symbolic refs** other than HEAD, and changing HEAD by a push (a `zen-git` command sets it)
* **over the public mirror (§13):** pushes, and shallow and partial clones (git's dumb HTTP protocol has none)

## 13. The public mirror

A repo meant to be public is published as a plain directory for git's **dumb HTTP** protocol, which stock git clones and fetches with no server-side git: `git clone https://<host>/unencrypted/<path>/<repo>.git`.

* **`zen-git publish <url> <dir>`** writes `<dir>/<repo>.git`:
  1. It syncs the member's mirror (§5) under the mirror's lock, and clones it bare (`git clone --bare --no-local`) into a temporary directory next to `<dir>/<repo>.git`.
  2. One pack (`git repack -a -d`), then `git update-server-info`, which writes `info/refs` and `objects/info/packs`. `HEAD` names `repos.head`.
  3. The temporary directory is swapped in by a rename, and the previous copy is removed after the swap. So a reader sees the old copy or the new one, never half of one, except for a fetch that started on the old copy and reads a pack after the swap, which fails and is retried.
* **Served by `/unencrypted`** (operations.md §9): with `<dir>` inside the server's `unencrypted_dir`, or as part of the site a `static push` uploads. A `static push` replaces the whole site, so the site's other files go in the same push.
* **`zen-git worker --publish <repo>=<dir>`** publishes again after each push: a `consumeManual` group `"publish"` on that repo's `pushes` topic, which acknowledges each event once its copy is written. Events that arrive during a publish are covered by the next one.
* **It is plaintext and public by design:** the repo's whole history, its refs and their names, readable by anyone who can reach the server. Only repos meant to be public are published.
* **Read only**, and no shallow or partial clones (§12). One full pack per publish: a fetch from the copy downloads the whole pack again when its `objects/info/packs` changes.
