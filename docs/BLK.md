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
