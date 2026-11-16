# Git over zen-serve

Git repositories kept in an encrypted fs, reached by stock git through a remote helper: `git clone zen://<host>/<fs>/<repo>`. The server stores packs and refs sealed, and never sees names, refs, oids or contents. The design is [spec/git.md](../spec/git.md). Milestone 10 builds it ([MILESTONE-10.md](MILESTONE-10.md)).

This page covers `git-remote-zen` (step 10.3), and the repack worker and the public mirror (step 10.4). The local server (`zen-git serve`) and the relayed service come in later steps.

## 1. Setting up

### 1.1 Once per fs (an admin)

1. **A device keyslot per member's device.** The helper unlocks the fs with the device in its vault, never with a passphrase (git.md §1). An admin adds each device's slot: `fs.addDevice(devicePublic)` (CLIENT.md, keyslots).
2. **The topic grants** (git.md §10). A push emits an event to `("git", repo, "pushes")`, which needs `append` there. Only an admin changes the ACL, so they grant it once per fs:
   ```
   zen-git setup zen://<host>/<fs> --device admin-device.json
   ```
   * It grants `append` on the topic prefix `("git")` to every member with fs `write`, and `read` and `consume` to every member with fs `read`.
   * It also grants `read` and `consume` on `("zen", "db", "git", "sched")` to every member with fs `write`: the scheduler's lease, which the repack worker (§4) needs for its delayed gc.
   * It unlocks the fs with the admin's device slot, or with `ZEN_PASSPHRASE` if set.
   * Running it again adds only what is missing. Run it again after adding members.

### 1.2 Once per machine (each member)

1. Install `@zen/git`. It puts three programs on `PATH`: `git-remote-zen`, `git-remote-zen+http` (the same program; git runs `git-remote-<scheme>`) and `zen-git`.
2. Put the device in the vault:
   ```
   zen-git login --server https://<host> --device my-device.json
   ```
   * The vault is `$ZEN_GIT_VAULT`, or else `~/.config/zen-git/vault` (`$XDG_CONFIG_HOME`). It is sealed under a key kept beside it (the device layer, CLIENT.md).
   * The device file can then go: the vault keeps the device and the session.
   * The helper never prompts. Git may run it with no terminal, so a vault with no device is an error naming `zen-git login`.

## 2. Using it

```
git clone zen://<host>/<fs>/<repo>            # over HTTPS
git clone zen+http://localhost:8080/1/<repo>  # plain HTTP: a local server, tests
git push origin main
```

* `<fs>` is the fs id in decimal. A repo name is 1 to 100 of `[A-Za-z0-9._-]`, not starting with `.`; a trailing `.git` is ignored.
* **A first push creates the repo**, with HEAD at the first branch it pushes. `zen-git head <url> <branch>` changes HEAD later.
* **What works** (with git 2.43 and 2.56):
  * clone, fetch and push, with `--force`, `--force-with-lease` and `--atomic`
  * deletes, light and annotated tags
  * shallow clones (`--depth`) and partial clones (`--filter=blob:none`)
  * sha1 and sha256 repos
* **Concurrent pushes:** of two pushes to one branch, exactly one wins. The other is refused as a non-fast-forward (`fetch first`), and a `pull --rebase` and push then works. Pushes to different branches both land.
* **Rights:** fs `read` to clone and fetch, fs `write` to push. A push without `write` is refused (`zen: push refused: forbidden: no write right on this fs`), with nothing changed. A grant covers the whole fs, so a repo with its own readers or writers lives in an fs of its own. Branch protection is cooperative only (TECH_DEBT TD-GIT-REF-POLICY).
* **CI:** a consumer of `("git", repo, "pushes")` (`db.consume`) gets one event per push, written in the push's own transaction (git.md §4):
  ```
  {1: repo, 2: device, 3: [{1: ref, 2: old oid | null, 3: new oid | null}], 4: pack hash | null, 5: seq}
  ```
  It opens the `git` database with the helper's schema: `SCHEMA` and `MIGRATIONS` in `packages/git/src/signin.ts` (as `packages/git/test/git.test.ts` does).

## 3. The mirror

The helper keeps one bare **mirror** per repo, at `~/.cache/zen-git/<host>/<fs>/<repo>.git` (`$XDG_CACHE_HOME`). Every clone of that repo on the machine shares it (git.md §6).
* Before each fetch or push, the helper syncs it from zen: the packs it lacks, then the refs. Then it runs git's own `upload-pack` or `receive-pack` on it. So shallow clones, partial clones and git's own push checks all work.
* **A push** goes through the mirror's `pre-receive` hook. The hook cuts the push's pack and writes it with the refs' compare-and-set in one transaction (git.md §4).
* **The mirror holds the repo's full plaintext history,** as a clone does. Use disk encryption.
* **It can be deleted** at any time: the next fetch rebuilds it from zen. It is also rebuilt by itself when zen's repo was recreated (another tree) or its object format differs.
* **Don't push into the mirror's directory directly.** Its hook refuses a push not made through the helper.
* **Locking:** a sync and a push hold `<repo>.git.lock/`, a directory holding the holder's pid. A lock whose pid is gone is taken over.

## 4. Repacking: `zen-git worker`

Every push adds one pack. Many packs make clones and syncs slower, so past 16 live packs a push asks for a repack (git.md §9), and a **worker** rolls the newest packs up into one.

```
zen-git worker zen://<host>/<fs> [--grace 24h] [--publish <repo>=<dir>]...
```

* **One per fs,** on an always-on machine, run by a member with fs `write` (its vault, as for the helper). It serves every repo of the fs, including repos created later. A second worker waits: the job is one consumer group with one lease.
* **What it does:** it rolls up the newest packs geometrically (as `git repack --geometric=2`), from those packs alone, in a temporary directory. Then one transaction marks them replaced and adds the new pack. Their files are deleted after `--grace` (24 h by default), so fetches that listed them before still finish.
* **It prints** one line per repack (`repo: 17 packs → 1, 0.07 MB, 0.27 s; 1 live`), per gc and per publish, to stderr.
* **Stopping it** (SIGINT, SIGTERM) releases its lease. If it dies mid-repack, nothing changes, and the next worker repacks once the lease lapses (10 s).
* **Without a worker,** packs pile up. Pushes still work, and the mirrors repack their own copies, but a fresh clone downloads every pack, and each sync lists them all.
* **`zen-git repack zen://<host>/<fs>/<repo>`** rolls a repo up once, now, whatever its pack count.
* **A systemd user unit,** for example:
  ```
  # ~/.config/systemd/user/zen-git-worker.service
  [Service]
  ExecStart=/usr/bin/env zen-git worker zen://zen.example.com/1
  Restart=on-failure
  [Install]
  WantedBy=default.target
  ```
  Then `systemctl --user enable --now zen-git-worker`, and `loginctl enable-linger` so it runs while you are logged out.
* **A mirror that keeps up never downloads a roll-up:** it already has the rolled packs' objects (git.md §5).

## 5. A public mirror: `zen-git publish`

A repo meant to be public can be published as a plain directory for git's dumb HTTP protocol, which stock git clones with no git on the server (git.md §13):

```
zen-git publish zen://<host>/<fs>/<repo> <dir>   # writes <dir>/<repo>.git
git clone https://<host>/unencrypted/<path>/<repo>.git
```

* **Serving it:** put `<dir>` inside the server's `unencrypted_dir` (operations.md §9), which `/unencrypted` serves as plain files. Or upload it with `static push`, which **replaces the whole site**: push the site's other files with it.
* **Keeping it current:** `zen-git worker --publish <repo>=<dir>` publishes again after each push.
* **It is plaintext and public:** the whole history, every ref and its name. Publish only what is meant to be public.
* **Read only,** with no shallow or partial clones (dumb HTTP has none). Each publish writes one full pack, so a fetch from the copy downloads the whole pack again.

## 6. Costs

These are measured in [STATS.md](STATS.md) §5.8.
* Each git command that reaches zen starts the helper (Node with the client's wasm, about 90–120 MB peak).
* A push also starts the hook, a second process of the same size. Its sign-in resumes the vault's session, so the server signs no new challenge.
* This repository's history (584 commits, 3.5 MB packed) pushes in about 1.8 s and clones in about 1.1 s. A plain `git clone` of it from a local bare repo takes 0.33 s.
* **The worker** keeps up with a push every 100 ms: about one repack per 16 pushes, each under a second or so, at 190–230 MiB peak. The live packs stay within 1.15× of one full `git repack -a -d`.

## 7. Not supported

These are git.md §12 and TECH_DEBT TD-GIT-SCOPE:
* push options (`git push -o`) and signed pushes (`--signed`): git refuses them with a clear message
* Git LFS
* submodules as one unit (each is a repo of its own)
* symbolic refs other than HEAD, and changing HEAD by a push

**One case to know about:** a member with fs `read` only can't create the `git` database. Their clone of an fs where nobody has pushed yet fails until a writer's first push.

## 8. Troubleshooting

* **`zen: no device in the vault …`:** run `zen-git login`, or point `ZEN_GIT_VAULT` at the vault.
* **`zen: fs N: this device can't unlock it`:** an admin adds the device's keyslot (§1.1).
* **`zen: push refused: fetch first`:** someone pushed first. Fetch, then merge or rebase, and push again.
* **`zen: push refused: forbidden …`:** the fs has no `write` for this member, or `zen-git setup` hasn't been run for the topic.
* **`zen: pack … has gone (repacked)`:** a sync listed packs that a repack's gc then deleted, twice in a row. Run the command again.
* **The live packs keep growing:** no worker runs for the fs (§4), or its member lacks the scheduler's grant (run `zen-git setup` again).
* **`ZEN_GIT_DEBUG=1`** prints the stack of an unexpected error. For the worker it also logs each failed repack attempt and each retried commit.
