# Milestone 11: an encrypted web-app platform (trust, sharing, lifecycle) (placeholder)

A placeholder added during milestone 5 (after step 7e). Milestones 8–11 come at the end of the roadmap ([DESIGN-3 §6](DESIGN-3.md)); their order follows their dependencies: 8 → 9 → 10 → 11, with milestone 6 (signed roots) before 11. The parts that are central, or that the work of milestone 5 would otherwise have to redo, moved earlier: see "Moved earlier" below and [`MILESTONE-5.md`](MILESTONE-5.md) steps 7f–7h and 8.

* **Feature:** what zen-serve still lacks to qualify as an encrypted-storage web-app server:
  * **Code-delivery trust:** signed app bundles, verified by a pinned bootstrap service worker (this reopens DESIGN-3 §0), plus SRI and reproducible builds, with a trust anchor the server doesn't control.
  * **Sharing:** share links that carry a file key in the URL fragment plus a scoped read capability (TD-AUTH-TOKEN-SCOPES); folder shares; sharing with outsiders.
  * **Lifecycle:**
    * member removal: revoke, `rotate`, and fresh file keys on the next write
    * account recovery
    * Web Push with encrypted payloads
    * log retention (TD-LOG-RETENTION)
    * server timers (TD-BROKER-SERVER-TIMERS)
  * **Metadata:** padding options for sizes and page counts.
  * **Assurance:** a threat model of web delivery, audit readiness, and the wasm size (TD-CLIENT-WASM-OPT).
* **Moved earlier:**
  * **7f:** per-file content keys. Sharing one file then needs no format change. The `zen/v1/sig/app-bundle` label is reserved.
  * **7h:** the vault.
  * **Milestone 6:** signed roots, the rollback half of integrity. Its signing keys are reused for bundle signatures.
  * **M9:** the worker that carries the bootstrap.
* **Plan for the rest:** the five areas above, probably split into sub-milestones when planned.

**The rest is planned when the milestone starts.**
