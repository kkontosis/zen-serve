# Milestone 8: large files (the rest) (placeholder)

A placeholder added during milestone 5 (after step 7e). Milestones 8–11 come at the end of the roadmap ([DESIGN-3 §6](DESIGN-3.md)); their order follows their dependencies: 8 → 9 → 10 → 11, with milestone 6 (signed roots) before 11. The parts that are central, or that the work of milestone 5 would otherwise have to redo, moved earlier: see "Moved earlier" below and [`MILESTONE-5.md`](MILESTONE-5.md) steps 7f–7h and 8.

* **Feature:** files of hundreds of GiB that are streamed, resumable and appendable.
* **Moved earlier:**
  * **7f/7g (the layout, file keys, streams).** Step 8's replica stores and replays the format, so changing it after step 8 would mean designing the write path twice.
  * **Resumable state in step 8.**
* **Plan for the rest:**
  * Upload sessions that pin chunks and pages past `chunk_grace_secs`, so a resume works after more than 24 h.
  * Repacking pages after many edits in the middle of a file (a leader job).
  * Multi-GiB performance, measured in STATS.
  * The interplay with `zen-ublk` (M6).

**The rest is planned when the milestone starts.**
