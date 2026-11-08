# Block devices

A guide for whoever runs a disk on zen-serve: `zen-ublk` serves a [zen-blk](../crates/zen-blk) volume as a Linux block device, so `mkfs.ext4` on it gives a disk whose contents the server can't read, change or roll back unnoticed. The normative description of volumes is [spec/blk.md](../spec/blk.md); the Rust client it is built on is in [CLIENT.md §13](CLIENT.md#13-the-rust-client).

## 1. What it is

* **A volume** is a fixed number of 4,096-byte blocks inside an fs, each one a sealed KV value. A Merkle tree over the blocks and a hash-chained root record make every read checkable, and the root's `seq` rides in device checkpoints, so a server serving an older root is caught (spec/blk.md §5–§6).
* **One writer at a time** holds the volume's lease; every commit is fenced with it, so a writer that lost the lease writes nothing. Read-only exports need no lease.
* **Two frontends** call one adapter:
  * **FUSE:** a directory with one file, `disk.img`, the volume, for `losetup`. Pure Rust (no libfuse); it mounts through `fusermount3`.
  * **ublk:** `/dev/ublkbN`, without a loop device. It needs the `ublk_drv` kernel module and a `zen-ublk` built with the `ublk` feature (libublk's bindings are generated at build time, which needs libclang).

## 2. Running it

The device key file and pins are the Rust client's (CLIENT.md §13): the same `--url`, `--device-key`, `--fs`, `--pins`, `--origin` and `--ca`. The device needs a keyslot in the fs and write access to it.

```
zen-ublk CONNECT --vol NAME [--vol-pin FILE] [--read-only] [--lease-ttl-ms MS] [--batch-blocks N]
         [--idle-flush-ms MS] [--io-timeout-ms MS] COMMAND
  create SIZE       create the volume (bytes, or with K/M/G/T; a multiple of 4,096), print its root
  info              the volume's size and root, as JSON
  import IMAGE [--create] [--progress FILE] [--restart]
                    copy a raw image (a file or a block device) into the volume, resumably
  export IMAGE [--force]
                    write the volume to IMAGE, a sparse file (read-only: no lease)
  fuse DIR          serve DIR/disk.img until DIR is unmounted or the daemon is signalled
  ublk [--id N] [--queues Q] [--depth D]     serve /dev/ublkbN until signalled  (feature "ublk")
```

**Over FUSE**, as root:

```sh
zen-ublk $CONNECT --vol disk create 20G
zen-ublk $CONNECT --vol disk fuse /run/zen-disk &          # prints "ready /run/zen-disk/disk.img"
dev=$(losetup -b 4096 -f --show /run/zen-disk/disk.img)
mkfs.ext4 "$dev" && mount "$dev" /mnt/disk
# … and to stop: umount /mnt/disk; losetup -d "$dev"; kill %1
```

Use `losetup -b 4096`: the volume's blocks are 4,096 bytes, and aligned I/O never reads a block in order to write part of it.

**Over ublk**, as root, after `modprobe ublk_drv`:

```sh
zen-ublk $CONNECT --vol disk ublk --id 0 &                  # prints "ready /dev/ublkb0"
mount /dev/ublkb0 /mnt/disk
```

`--id N` makes the device name stable. If a killed daemon left device N behind, the next `zen-ublk ublk --id N` deletes it first. If N is still served, it exits with `busy`.

**Read-only exports** (`--read-only`) need no lease and refuse writes (`EROFS`). Mount them `-o ro,noload` (ext4 would otherwise replay its journal). A read-only export reads one root. When the writer moves it, reads that need the new root fail with `EIO`, and the message says to restart the export (TD-BLK-SNAPSHOTS: pinned snapshots).

**Moving images in and out.**
* **`import IMAGE`** copies a raw disk image into the volume. The image is a file or a block device of exactly the volume's size; with `--create`, the volume is created at the image's size.
  * **Empty blocks:** holes in the image and blocks of zeros aren't stored. On a volume the import created there is nothing to store there, and on an existing volume they are discarded, so the volume ends equal to the image.
  * **Commits:** the image goes in windows of `--batch-blocks` blocks (1 MiB by default, within the server's limits), each one commit.
  * **Resuming:** after each window, a progress file (the volume pin's path + `.import`, or `--progress FILE`) records how far it got and the root it left. Run the same command again to continue from there. That works if the volume is still at that root, or one commit past it by this device (a crash between a commit and its record).
  * **Refusals:** if the volume or the image changed since, the import refuses and says so. `--restart` starts over, discarding what the image doesn't have.
  * **Output:** it prints the volume's root at the end.
* **`export IMAGE`** writes the volume to a new file (`--force` replaces one).
  * **Read-only:** it needs no lease, so it can run while a writer serves the volume, and it reads the root it opened, checked as every read is.
  * **Holes:** blocks never written or written with zeros are holes in the file, so a mostly empty 1 TiB volume exports to a small file.
  * **Atomic:** it writes `IMAGE.part` and renames it at the end, and prints the root it exported.
  * **A volume in use:** if the writer moves the root meanwhile, the export fails and leaves nothing. Stop the writer, or freeze its filesystem (`fsfreeze -f`), and export again (a consistent export of a live volume waits for snapshots, TD-BLK-SNAPSHOTS).
  * **Block devices:** it writes regular files only. To put an image on a device, export it, then `dd if=IMAGE of=/dev/sdX bs=1M` (not `conv=sparse`: on a device, a skipped block keeps what was there).

```sh
zen-ublk $CONNECT --vol vm import --create vm.raw     # resumable: run it again after an interruption
zen-ublk $CONNECT --vol vm export vm-backup.raw       # sparse; prints {"size":…,"seq":…,"root":…}
```

**Stopping.** SIGINT or SIGTERM, or unmounting the FUSE directory, stops the daemon. It then flushes, writes a checkpoint and releases the lease. A FUSE directory still in use (a loop device on it) can't be unmounted: the daemon says so and keeps serving. Detach the loop device first.

**Exit codes:**
* 0: a clean stop
* 3: an integrity alarm, printed as JSON on stderr
* 1: anything else, such as `busy` (another writer holds the volume) or a lost lease

## 3. Durability

The disk reports a **volatile write cache**, as most disks do:
* Writes are buffered in the daemon.
* A flush returns once they are committed. That means FLUSH, a FUA write, `fsync` on the FUSE file, or a loop device passing ext4's barriers on.
* A full batch (`--batch-blocks`, default 256 blocks) commits on its own, and so does an idle volume after `--idle-flush-ms` (default 5 s).

So a crash, including the daemon being killed, loses only writes no flush covered, as a power cut does to a disk's cache. Any subset of them may have reached the server. ext4's journal handles exactly that: after a crash, `e2fsck` replays the journal, and everything that was synced is there. The tests do this: `sync`, more writes, SIGKILL, `e2fsck`, then compare.

**One writer.** A second writer gets `busy` until the first one's lease lapses (`--lease-ttl-ms`, default 30 s, renewed every 10 s). So after a crash, the restart waits that long. A writer whose lease lapsed and was taken over writes nothing more (fenced). From then on every request gets `EIO`.

**The network.** A request that can't reach the server, or gets a 5xx, retries with backoff for up to `--io-timeout-ms` (default 60 s), then gets `EIO`. A flush that failed keeps its blocks, so the next one commits them. A commit whose answer was lost is settled from the stored root before anything else is read or written: if it applied, the volume continues from it, and it is never written twice at one seq.

## 4. Integrity

* **Every read is checked** against the tree. The pins file and the volume's root pin (`--vol-pin`, default: the key file's path + `.vol-<fs>-<hex(name)>`) carry what the device verified from one run to the next. Keep them with the device key.
* **An alarm stops the disk:** a rollback, a fork, or a checkpoint that doesn't verify (formats.md §14). The alarm is printed once, and every request gets `EIO` from then on: the disk **fails closed**. The device stays until the daemon is stopped, since removing it under a mounted fs is worse. Then it exits 3.
* **A lost lease fails closed the same way** (exit 1).
* **The window:** a server can serve a root one checkpoint old to another device, between a write and its writer's next checkpoint (spec/blk.md §6). The writer itself catches any rollback of its own writes on its next open, through its root pin.

## 5. How it works

A summary of [spec/blk.md](../spec/blk.md), which is normative.

* **Keys.** A volume's values live under the stored key of `("blk", name)`: the root record, the blocks, the tree nodes and the lease name. Stored keys are PRF tokens (formats.md §3.1), so the server sees neither the name nor a block's index, and neighbouring blocks don't sort together. Every value is sealed with its stored key in the AAD, so a block can't be moved to another index, volume or fs; it can only be replaced by an older value of the same key.
* **Leaves.** A block's leaf hashes its index and its whole sealed value; an absent block (which reads as zeros) has a zero leaf. Any older value of a block is therefore another leaf, with no write counter needed.
* **The tree.** Fan-out 128: a node is the 4,096 bytes of its 128 child hashes. A 1 GiB volume has depth 3, a 1 TiB one depth 4. A node whose children are all zero isn't stored, so an empty volume stores only its root record, and storage follows what was written.
* **The root record** `{seq, prev, blocks, root, writer, hlc}` is hash-chained: `prev` is the hash (RH) of the previous record.
* **A commit** carries the blocks written or deleted, every node on their paths whose hash changed, the next root record and the lease's fence. It reads nothing; the fence keeps other writers out. So each commit moves the volume from one verified state to the next, atomically.
* **A read** fetches the block together with any nodes of its path the daemon doesn't hold, in one request, and checks them top-down from the verified root: each node against its parent's slot, the block against its leaf. The writer keeps every node above level 1 and a cache of level-1 nodes (4,096, 16 MiB).
* **A lost answer** (the server applied a commit, or didn't, and the reply was lost) is settled from the stored root before the daemon reads or writes again, so a volume never gets two root records at one `seq`.

## 6. What the server sees and can do

**It sees:**
* how many blocks and nodes are stored, which of them each commit touches, and when
* how often the writer flushes, and how large each commit is
* discards, as deletes

It doesn't see the name, a block's index or the contents. The fixed block size hides write lengths. (spec/blk.md §7.)

**It can't** read a block, forge one, or move one to another index or volume: each is sealed under the fs key, bound to its stored key, and checked against the tree.

**What it can try, and what catches it:**

| The server … | Caught by |
|---|---|
| serves an older block, node or root to the writer | the writer's root pin, on its next open (even with no checkpoint yet); a block or node that doesn't match the root at once |
| serves an older root to another device | that device's floor, once it has synced a checkpoint of the writer that carries a newer root |
| shows two devices two histories (a fork) | the same `seq` with another RH, once checkpoints cross between them, or when pins are compared out of band ([INTEGRITY.md](INTEGRITY.md) §7) |
| withholds the writer's newest checkpoints | the writer's own sync (its head below its pin) |

**The window:** a read-only export on another device, opened between a commit and the writer's next checkpoint, can be shown the older volume. The cadence bounds it (by default, 64 changes or 5 s after the last one); a daemon's close always writes a checkpoint.

## 7. Numbers

The engine in process (docs/STATS.md §5.7) writes 49 MiB/s sequentially, reads a random block in 0.14 ms, and flushes one block in 1.6 ms. Through ext4 (STATS.md §6.3):

| ext4 on a 1 GiB volume (CI runner) | FUSE + loop | ublk |
|---|---|---|
| Write 256 MiB, `conv=fsync` | 48 MiB/s | 52 MiB/s |
| Read 256 MiB, cold | 253 MiB/s | 315 MiB/s |
| A synchronous 4 KiB write (`oflag=dsync`) | 4.5 ms | 4.3 ms |
| Untar ~240 files, then `sync` | 0.18 s | 0.12 s |

On the 4-core development machine, FUSE + loop measured 18–25 MiB/s, 113–140 MiB/s and 11.7 ms. So ublk is a little faster than FUSE + loop, and both are bounded by the server's round trips: a disk for a filesystem that syncs a few times a second, not for a database that fsyncs every write.

**Tuning:**
* **`--batch-blocks`** (default 256, 1 MiB) is a commit's size and the buffered blocks that force a flush. Larger batches commit less often (each commit rewrites the top of the tree and the root record), but a flush then waits for a larger commit, and the daemon holds more unflushed data. A commit's blocks are also capped by the server's limits, budgeting a full path of new nodes per block: with the default `max_commit_bytes` (8 MB), about 420 blocks at depth 3 (volumes up to 8 GiB) and 340 at depth 4.
* **`--idle-flush-ms`** (default 5,000) bounds how long written data waits in the daemon when nothing flushes it. ext4 flushes on its own every 5 s (`commit=5`) and on every `fsync`, so this matters mostly for direct writes to the FUSE file. Lower it for a disk that's written rarely and must lose little; `0` leaves flushing to the filesystem.

## 8. Files or volumes

(A single large file through zen-mount, rather than an image under a filesystem, is measured in [STATS.md §7](STATS.md#7-large-files-measured).)

The other way to keep a disk image in zen-serve is as a file of a zen-fs tree: [zen-mount](CLIENT.md#9-zen-mount) mounts the tree, and a loop device runs on the image file in it. The same ext4 workload on both (`tests/devices.rs`, `bench_fuse`, `bench_file_cdc`, `bench_file_fixed`; release build, an embedded server in the test process, the development container: 4 cores, loopback, one run each; the file columns after milestone 8 step 6b's memory bounds):

| ext4 on a 1 GiB image | volume (FUSE + loop) | file, fixed chunks | file, CDC |
|---|---|---|---|
| `mkfs.ext4` | 0.10 s | 5.0 s | 75 s |
| Write 256 MiB, `conv=fsync` | 25.6 MiB/s | 7.6 MiB/s | 0.8 MiB/s |
| Read 256 MiB, cold | 149 MiB/s | 24 MiB/s | 16.4 MiB/s |
| A synchronous 4 KiB write (`oflag=dsync`) | 8.3 ms | 146 ms | 2,905 ms |
| Untar 245 files (4.6 MiB), then `sync` | 0.21 s | 0.86 s | 32 s |
| The daemon's stop | 0.02 s | 15.9 s, 428 writes left unsent | 39.1 s, 314 writes left unsent |
| Its peak RSS | 62 MiB | 446 MiB (426–621 over three runs; 1,822 before step 6b) | 575 MiB (560 in another run; 1,544 before) |
| Stored on the server at the stop | 279 MiB | 1,338 MiB | 10,246 MiB |

**What makes the file slower:**
* **An fsync is a version.** The loop device passes ext4's barriers on as `fsync` on the file, and each is a flush of zen-mount's handle: an edit of the 1 GiB file, sealing the pages and chunks around what changed into a new version (CLIENT.md §8). A volume flushes the dirty blocks and the top of its tree.
* **Under CDC an edit re-cuts its neighbourhood:** about 3 chunks of ~32 KiB and their pages for a 4 KiB write, until the cuts resync. Fixed chunks cost one chunk. The figures above show it by an order of magnitude, so **an image file wants `chunking: fixed`** (`fs.setPolicy`, or a tree of its own in an fs with that policy).
* **The image's zeros are content.** zen-fs files have no holes (TD-FS-SPARSE), so `truncate -s 1G` seals and uploads a gigabyte of zeros at the first flush. A volume stores only blocks written.
* **Superseded versions stay until their grace passes** (a day by default), so the server holds every flushed version's new chunks until then: the stored column above. A volume overwrites blocks in place.
* **The queue lags.** zen-mount's fsync is durable on the device (`--cache-dir`) and sent later (CLIENT.md §9). Under this load it fell behind: at the stop, 400 to 450 writes were still queued (kept for the next mount, as zen-mount does after its 10 s wait). A volume's fsync returns once the server has the blocks.
* **Memory** (spec/OPS.md §6):
  * **Before milestone 8 step 6b,** zen-mount peaked at 1.5–1.9 GB under this load, for two reasons:
    * The replica writes its records to the cache dir behind, one file and fsync each, and nothing waited for it, so up to 951 MB of sealed content queued in memory.
    * The client's page memo kept up to 256 decoded pages. Each is about 1 MB of small objects, and every fsync made new ones.
  * **Now both are bounded:**
    * Sealing waits once 32 MiB of records are waiting (`--pending-size`), and content fetched for reads isn't kept past that.
    * The memo keeps 65,536 chunk ids, about 12 MB (`--page-memo`).
    * What's live at the peak is the handle's written data (up to `--dirty-size`, 64 MiB), the records waiting, and wasm memory. The RSS beyond that is the allocator keeping what it freed.
  * **The price:** writes now go at the cache disk's pace. Under CDC the first flush of the image's gigabyte of zeros takes most of `mkfs`'s 75 s (3.5 s before, when it went to memory), and a 4 KiB synchronous write takes 2.9 s instead of 2.2. A larger `--pending-size` trades memory back for that.

**Which to use:**
* **A volume** to run a filesystem: fsync means the server has it, reads are checked per block against a Merkle root, the root's freshness rides in checkpoints, and one writer at a time holds a fenced lease. It has a fixed size and no history yet (TD-BLK-SNAPSHOTS).
* **A file** to keep or carry an image rather than run on it: any zen-fs client can read it, it works offline, its versions are history until collected, and identical chunks are shared with other files of the fs. Write it whole (`writeFile`, or `cp` into a zen-mount), under `chunking: fixed`, not through a loop device. Two devices writing it at once leave siblings (no lease).
* **To move between them:** `export` a volume to a raw image and copy it into zen-fs, or take an image out of zen-fs and `import` it (§2). Snapshots of volumes kept as zen-fs files are TD-BLK-SNAPSHOT-FILES.

`import` and `export` (`bench_image`): a 1 GiB ext4 image with 294 MiB allocated (the source tree and 256 MiB of random data) imports in 8.0 s (37 MiB/s of data) and exports in 2.7 s (108 MiB/s), to a file with 262 MiB allocated; the volume stores 271 MiB.

## 9. Limits

* **One writer per volume.** Others can export it read-only.
* **A fixed size,** set at creation. No snapshots, clones or deletion yet (TD-BLK-SNAPSHOTS).
* **Read-only exports read one root.** When the writer moves it, reads that need the new root fail with `EIO` (`volume_changed`); restart the export (pinned snapshots: TD-BLK-SNAPSHOTS). The same holds for `export IMAGE`: it fails if the writer moves the root meanwhile.
* **ublk** needs a `zen-ublk` built with `--features ublk` (libclang at build time, for libublk's bindings) and the `ublk_drv` module at run time. The FUSE frontend needs neither: only `/dev/fuse`, `fusermount3` and a loop device.
* **Throughput** is bounded by round trips, not by crypto: a flush is a commit, and reads that miss the page cache are a request each. It's a disk for a filesystem that syncs a few times a second, not for a database that fsyncs every write (STATS.md §6.3).

## 10. Running it under systemd

A unit for the ublk frontend, and a mount that needs it:

```ini
# /etc/systemd/system/zen-disk.service
[Unit]
Description=zen-ublk volume "disk"
Wants=network-online.target
After=network-online.target

[Service]
ExecStartPre=/sbin/modprobe ublk_drv
ExecStart=/usr/local/bin/zen-ublk --url https://zen.example.org --device-key /etc/zen/disk-key.json \
          --fs 1 --vol disk ublk --id 0
ExecStartPost=/bin/sh -c 'until [ -b /dev/ublkb0 ]; do sleep 0.2; done'
KillSignal=SIGTERM
# Above the flush and its retries (--io-timeout-ms, 60 s by default).
TimeoutStopSec=90
Restart=on-failure
# After a crash the lease (30 s) must lapse before a restart can write.
RestartSec=35
# An integrity alarm (exit 3) isn't fixed by restarting: leave it stopped.
RestartPreventExitStatus=3

[Install]
WantedBy=multi-user.target
```

```ini
# /etc/systemd/system/mnt-disk.mount
[Unit]
BindsTo=zen-disk.service
After=zen-disk.service

[Mount]
What=/dev/ublkb0
Where=/mnt/disk
Type=ext4

[Install]
WantedBy=multi-user.target
```

* **`--id`** keeps the device name stable, so the mount unit can name it. After a SIGKILL or a crash, the dead device stays until it is deleted; the next start with the same `--id` deletes it first.
* **`KillSignal=SIGTERM`**: the daemon stops the device, flushes, checkpoints and releases the lease. `BindsTo` and `After` make systemd unmount the filesystem first.
* **The pins** (`--pins`, `--vol-pin`) default to files next to the device key, so `/etc/zen` must be writable by the service, or point them elsewhere (`/var/lib/zen`). Losing them means the next start trusts what it is first shown ([INTEGRITY.md](INTEGRITY.md) §7).
