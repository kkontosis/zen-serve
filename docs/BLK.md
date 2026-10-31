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

## 8. Limits

* **One writer per volume.** Others can export it read-only.
* **A fixed size,** set at creation. No snapshots, clones or deletion yet (TD-BLK-SNAPSHOTS).
* **Read-only exports read one root.** When the writer moves it, reads that need the new root fail with `EIO` (`volume_changed`); restart the export (pinned snapshots: TD-BLK-SNAPSHOTS).
* **ublk** needs a `zen-ublk` built with `--features ublk` (libclang at build time, for libublk's bindings) and the `ublk_drv` module at run time. The FUSE frontend needs neither: only `/dev/fuse`, `fusermount3` and a loop device.
* **Throughput** is bounded by round trips, not by crypto: a flush is a commit, and reads that miss the page cache are a request each. It's a disk for a filesystem that syncs a few times a second, not for a database that fsyncs every write (STATS.md §6.3).

## 9. Running it under systemd

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
