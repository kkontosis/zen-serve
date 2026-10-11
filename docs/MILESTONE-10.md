# Milestone 10: git over zen-serve (`git-remote-zen`) (placeholder)

A placeholder added during milestone 5 (after step 7e). Milestones 8–11 come at the end of the roadmap ([DESIGN-3 §6](DESIGN-3.md)); their order follows their dependencies: 8 → 9 → 10 → 11, with milestone 6 (signed roots) before 11. The parts that are central, or that the work of milestone 5 would otherwise have to redo, moved earlier: see "Moved earlier" below and [`MILESTONE-5.md`](MILESTONE-5.md) steps 7f–7h and 8.

* **Feature:** `git clone zen://host/<fs>/<repo>` through a remote helper written on the client. The server stays blind.
  * Packs (with their `.idx`) are immutable files.
  * Refs live in a zen-db table.
  * A push is one transaction: the pack's file op, a compare-and-set of the refs (non-fast-forward refused on the client unless `--force`), and an emit to `("git", repo, "pushes")` for CI workers.
  * Fetch downloads only the packs it lacks.
  * Read and write rights are the fs grants. Branch protection is cooperative only.
* **Moved earlier:**
  * **7f/7g:** packs over 175 MiB, and streaming uploads.
  * **7h:** the helper signs in without a prompt.
  * Already built: files plus rows in one commit (5c), emits (6a), and the scheduler for repacking (6b).
* **Plan for the rest:**
  * `packages/git` (a Node helper binary)
  * the refs schema
  * a pack manifest for large repos
  * a gc/repack leader job
  * an optional localhost smart-HTTP bridge (`insteadOf`)
  * a dumb-HTTP public mirror through M9's static push
  * tests with real `git`: clone, push, concurrent pushes, force

**The rest is planned when the milestone starts.**
