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
* The helper and the local server sign in through the vault as the member's device (CLIENT.md, `credentialVault`), without a prompt.

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
* A pack is **written once**, by the transaction that adds its row (§4), and never changed. It is deleted only by a repack's gc (§9).
* Every pack is **self-contained**: never thin, so each of its deltas has its base in the same pack. Its objects may refer to objects in older packs (a commit's parents, say).
* The `.idx` is git's index of the pack. Readers don't trust it: an import rebuilds it (§5). It is stored for readers that read objects from a pack directly, such as the dumb-HTTP mirror (10.4).
* The fs's chunking policy applies (fs.md §4.1). Content-defined chunks, the default, save about a fifth of the bytes between two full repacks of one repo, and little between small packs and their repack (the spike). So repacks roll small packs up instead of rewriting everything (§9).

### 2.3 Object format

A repo is `sha1` or `sha256` (git's `--object-format`), fixed when it is created by its first push. Every mirror of it is created with that format. Oids are stored as raw bytes: 20 or 32.

## 3. The database

Every repo of an fs is in one zen-db database of that fs, `ns = "git"` (zendb.md §2.1), `authenticated` by default (zendb.md §14), which needs integrity attached to the fs (formats.md §14); without it, `basic`. Every table is `pad: true` (zendb.md §4.3).

| Table | Primary key | Fields |
|---|---|---|
| `repos` | `name` | `format` (`"sha1"` or `"sha256"`), `head` (a ref name, HEAD's target), `tree` (16 bytes), `next_seq` (u64), `created_ms`, `creator` (device id) |
| `refs` | `[repo, ref]` | `oid` (bytes), `device` (the pusher's device id), `time_ms`, `seq` (the push's, below) |
| `packs` | `[repo, hash]` | `seq` (u64), `size` (bytes), `objects` (count), `nodes` (`[pack, idx]` node ids), `state` (`"live"` or `"replaced"`), `replaced_ms`, `by` (the hash of the pack that replaced it) |

* `refs` and `packs` have a private index on `repo` (zendb.md §5.4), so that one repo's rows are read without reading the others'.
* `seq` orders a repo's packs by push. Each push takes `next_seq` from its `repos` row and increments it (§4), so a pack's objects refer only to objects in packs of a lower `seq`, or in the same pack.
* HEAD is `repos.head`, set by the first push to the first `refs/heads/` ref it pushes (as a stock server's first push does). No other symbolic refs are kept.
* **Why `authenticated`:** git's objects check themselves by their oids, so the refs are what a server could serve stale. In a `basic` database it could serve an older ref, or leave a ref out of a listing (zendb.md §14.1); an `authenticated` database detects both, up to the checkpoint window (zendb.md §14.7). A `basic` database is allowed, and loses that.

## 4. A push

A push is cut as one pack (or none) and applied in one transaction:

1. **The pack.** `git pack-objects --revs --stdout` of the pushed tips, excluding (`^`) every tip zen had when the mirror was synced, never `--thin`. Then `git index-pack --strict` in the mirror, which checks every object and writes the `.idx`. A pack with no objects (a push of known objects, or of deletes only) is dropped.
2. **The upload.** The pack's and the index's chunks go up under an upload session (api.md §12.1), before the transaction, so a large pack's transaction carries only its `write`s (zendb.md §7.6). An upload interrupted here leaves nothing visible, and the session keeps what it uploaded for a resume.
3. **The transaction**, a manual one (zendb.md §7.2):
   1. Read the `repos` row (a new repo has none: it is created here) and every `refs` row the push updates.
   2. Each ref's current oid must equal the push's old value (the mirror's, which `receive-pack` gives the hook): absent for a new ref. If one differs, another push landed since the sync, and the hook refuses every ref it was given (`fetch first`).
   3. Write the pack's two files into the repo's tree, and put its `packs` row with `seq = next_seq`. Put the `repos` row with `next_seq + 1`.
   4. Put each updated `refs` row, or delete it for a deleted ref.
   5. Emit one event to the topic `("git", repo, "pushes")` (zendb.md §11.1):
      ```
      {1: repo, 2: device, 3: [{1: ref, 2: old oid | null, 3: new oid | null}], 4: pack hash | null, 5: seq}
      ```
   6. Commit.
4. **A conflict** at commit (a concurrent push read or wrote the same rows):
   * The hook re-reads and retries when every updated ref still has its old value: only the `repos` row moved, as when two pushes update different branches. The pack is reused, and its `seq` is taken again.
   * Otherwise the push is refused (`fetch first`), and git reports it as it reports a non-fast-forward.
* **Exactly one of two racing pushes to one ref wins** (spike: two clones pushing at once).
* **Fast-forward, `--force`, `--force-with-lease` and `--atomic`** are checked by `receive-pack` against the mirror's refs, which equal zen's refs read in step 3.1. So they are git's own checks. A push that is not `--atomic` but refused for one ref is applied for the others, as git does.
* **The window after the hook.** The hook commits before `receive-pack` updates the mirror's refs. If that update fails (a crash, a full disk), zen has the push but git reports `failed to update ref`, and the next sync (§5) brings the mirror to zen's refs. So a push reported as failed may have happened, as on a stock server whose ref update fails after its hook. To keep ref locks out of that window, a mirror is held exclusively for the whole `receive-pack` (§6). A `reference-transaction` hook would hold git's locks, but it runs once per ref unless the push is `--atomic`, which would split one push into several transactions (the spike).

## 5. A fetch: syncing a mirror

Before `upload-pack` or `receive-pack` runs, the mirror is synced:

1. **One read transaction** reads the `repos` row, the repo's `refs` rows, and its live `packs` rows. That is one consistent state.
2. **The packs the mirror lacks** are downloaded in `seq` order. Each is imported with `git index-pack --strict` inside the mirror, which rebuilds its `.idx` and checks every object and its links to the packs already imported. Hence the order: `--strict` refuses a pack whose objects refer to objects the repository doesn't have yet. The mirror records the imported packs' hashes, so it can repack its own files (§6) and still know what it holds.
3. **The mirror's refs are set to zen's:** refs absent from zen are deleted, and the others set to zen's oids (`git update-ref --stdin`). HEAD is set to `repos.head`.
4. **Then git's service runs** on the mirror.

* **A pack that fails to import** (a bad object, a missing base) stops the sync with an error naming the pack. Nothing is served from a mirror that failed to sync.
* **Shallow and partial clones** (`--depth`, `--filter`) work, since `upload-pack` serves them from the mirror. The mirror itself is always complete.

## 6. The mirror

* A bare repository with the repo's object format, kept on the machine that serves git:
  * the helper's, per user: `$XDG_CACHE_HOME/zen-git/<fs>/<repo>.git` (`~/.cache` by default), so every clone of a repo on the machine shares one
  * the local server's and the handler's: the same layout, under their own data directory
* Its config: `uploadpack.allowFilter true`, `uploadpack.allowAnySHA1InWant true`, `receive.unpackLimit 1`, and the `pre-receive` hook of §4.
* **Locking:** a sync and a `receive-pack` hold the mirror exclusively (a lock file next to it); `upload-pack` holds it shared. So no two pushes on one machine meet in one mirror, and none of git's ref locks is contended.
* **It repacks itself** when it holds more than 16 packs (`git repack -a -d`, local only). That kept 4 MB of the 28 MB that 585 small packs took (the spike).
* **Git's environment:** git sets `GIT_DIR` (and may set `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_PREFIX`, `GIT_COMMON_DIR`, `GIT_NAMESPACE`) for the helper. Those are removed for every git command run on the mirror, or `GIT_DIR` would override `-C`.
* **Plaintext:** the mirror holds the repo's full plaintext history, as a clone does. The docs say so, and recommend disk encryption.

## 7. `git-remote-zen`

The remote helper (gitremote-helpers(7)) for `zen://` URLs. It speaks only:

* **`capabilities`** → `connect`, `option`
* **`option <name> <value>`** → `ok` for `verbosity` and `progress`, `unsupported` for the rest (git then keeps its defaults)
* **`connect <service>`**, for `git-upload-pack` and `git-receive-pack`: it syncs the mirror (§5), answers an empty line, and then runs the service on the mirror with its own stdin and stdout, which git talks to directly.
  * **Protocol v2:** git gives a `connect` helper no `GIT_PROTOCOL`, so the helper starts `git-upload-pack` with `GIT_PROTOCOL=version=2`, unless git's `protocol.version` is 0 or 1. Git's client detects the v2 advertisement. Without it, `push.negotiate` fails with a warning (the spike).
  * The helper reads its commands one byte at a time, so nothing after the `connect` line is consumed before the service takes over.
* The `receive-pack` service's `pre-receive` hook does §4's transaction, signed in as the helper's device.

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

Implemented in 10.4. The rules:

* **When:** a repo with more than 16 live packs. A repack job runs in any client of a member with fs `write`, as the consumer of a `sequential` group `"repack"` on the repo's `pushes` topic (zendb.md §11.3), so one runs at a time per repo.
* **What:** a geometric roll-up (factor 2, as `git repack --geometric=2`) of the newest packs: the shortest run of the highest `seq` values such that every pack left has at least twice the bytes of the packs after it together. Rolling up only the newest keeps §3's order: the new pack's objects refer only to packs of a lower `seq`. In the spike, 585 small packs rolled up into one in 0.94 s.
* **Its transaction:** upload the new pack (§4 step 2), then read the rolled-up packs' rows. Every one must still be `live`. Mark each `replaced` (with `replaced_ms` and `by`), and put the new pack's row, `live`, with `seq` = the highest rolled-up `seq`. Refs don't change.
* **Gc:** a delayed message (zendb.md §12.4) at `replaced_ms` + 24 h has the job delete the replaced packs' files and rows. Until then, a fetch that listed them before the repack still finds them. A mirror that imported them keeps their objects, and imports the new pack at its next sync like any other (`index-pack` accepts objects the repository already has).

## 10. Rights

* **The fs grants** (formats.md §9.1): `read` to fetch, `write` to push and repack. A grant covers the whole fs, so a repo with its own readers or writers lives in an fs of its own.
* **The topic:** a pusher needs `append` on `("git", repo, "pushes")`, or the push's transaction fails (403) and nothing is pushed. CI consumers need `read` and `consume` there. Creating a repo grants these to the fs's writers and readers.
* **Branch protection, force-push rules and protected tags are cooperative.** The local server and handlers enforce what the repo's rules say for pushes through them. A member with fs `write` can still write any ref with a client of their own: the server can't read refs, so it can't check them (TECH_DEBT TD-GIT-REF-POLICY).

## 11. What the server sees

* **Pushes:** when a device pushes to the fs, the sizes of each pack's chunks and pages and its commit, the number of rows changed in padded buckets, and the event. It learns that two pushes touched the same rows when they conflict.
* **Fetches:** which pack chunks a device fetches and when, so how much was new to it.
* **Repacks:** a run of rows marked replaced and a larger pack written, then files deleted a day later.
* **Which repo, unnamed:** each repo is a tree of its own (§2.2) with a topic of its own, so the server sees how many repos an fs has, and which one each push, fetch and repack touches, without their names.
* **Never:** repo names, ref names, oids, file names (all sealed), or contents.
* **The relayed service:** what svc.md §12 lists: the TCP connection's endpoints, timing and byte counts, and which handler serves it.

## 12. What isn't supported

From the spike's feature matrix (docs/MILESTONE-10.md, step 10.2): clone, fetch (also `--depth` and `--filter`), push (also `--force`, `--force-with-lease`, `--atomic`), deletes, light and annotated tags, HEAD, and sha1 and sha256 repos all work. Not supported (TECH_DEBT TD-GIT-SCOPE):

* **push options** (`git push -o`): `receive.advertisePushOptions` is off, so git refuses them ("the receiving end does not support push options")
* **signed pushes** (`git push --signed`): no `receive.certNonceSeed`, so git refuses them ("the receiving end does not support --signed push"); `--signed=if-asked` pushes unsigned
* **Git LFS**, and **submodules** as one unit (each submodule is a repo of its own, with its own URL)
* **symbolic refs** other than HEAD, and changing HEAD by a push (a `zen-git` command sets it)
