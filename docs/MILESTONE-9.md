# Milestone 9: serving zen-fs over HTTP through a service worker (`@zen/sw`) (placeholder)

A placeholder added during milestone 5 (after step 7e). Milestones 8–11 come at the end of the roadmap ([DESIGN-3 §6](DESIGN-3.md)); their order follows their dependencies: 8 → 9 → 10 → 11, with milestone 6 (signed roots) before 11. The parts that are central, or that the work of milestone 5 would otherwise have to redo, moved earlier: see "Moved earlier" below and [`MILESTONE-5.md`](MILESTONE-5.md) steps 7f–7h and 8.

* **Feature:** a worker maps URLs in its scope to fs nodes and answers each fetch with a decrypted, streamed `Response`, with `Range` → 206, MIME types and index pages. The HTML stays unchanged. It works offline from the replica. An encrypted website is then viewable by the members who unlocked it.
* **Moved earlier:**
  * **7g:** `readStream`.
  * **7h:** the vault, so the worker re-unlocks after the browser kills it.
  * **Step 8:** a worker-safe `@zen/fs` with stream reads, the content-type xattr, and sealed chunks at rest. This is so the worker never caches plaintext.
* **Plan for the rest:**
  * the `packages/sw` worker: URL → node, streams, Range, MIME, directories; lifecycle with `clients.claim` and updates
  * **origin isolation**, so untrusted fs content never runs on the origin that holds keys:
    * a separate content origin, with its own config in `zen-serve`
    * `CSP: sandbox` and `nosniff` on every worker response
  * `zen-serve static push` (DESIGN-3 §4.2, replicated fs 0), and `Range` in `statics.rs`
  * the worker as the owner candidate of TD-DB-SHAREDWORKER
  * Playwright tests: offline reads, seeking, the sandbox
* **Note:** DESIGN-3 §0 made the worker "an optional cache only". M9 makes it a serving path. Whether the worker also verifies code is a question for M11.

**The rest is planned when the milestone starts.**
