#!/bin/sh
# Install the pinned FoundationDB release (client library + server binaries)
# from Apple's GitHub releases, verified by sha256. These are Apple's
# optimized, stripped release builds.
#
#   scripts/install-fdb.sh [OPTIONS] [DEST]     (default DEST: /opt/foundationdb)
#
# Every component is installed by default. Options:
#   --no-backup       skip fdbbackup, fdbrestore and backup_agent
#                     (zen-serve's `backup` and `restore` commands need them)
#   --no-dr           skip fdbdr and dr_agent (disaster-recovery replication,
#                     which zen-serve doesn't use)
#   --no-fdbmonitor   skip fdbmonitor (zen-serve supervises fdbserver itself)
#   --dedupe=MODE     how to store the backup/DR tools, which are one program
#                     under five names (it picks its role from the name it is
#                     run as): auto (default), reflink, hard, soft or none.
#                     auto tries a reflink (copy-on-write clone: btrfs, XFS,
#                     ZFS, bcachefs), then a hard link, then a symlink, and
#                     falls back to a copy. Every name stays and works.
#
# The .deb packages are unpacked into DEST (no services are started):
#   DEST/usr/sbin/fdbserver, DEST/usr/bin/{fdbcli,fdbbackup,fdbrestore,fdbdr,dr_agent},
#   DEST/usr/lib/foundationdb/{backup_agent/backup_agent,fdbmonitor}, DEST/usr/lib/libfdb_c.so
# When run as root, libfdb_c.so is also linked into /usr/lib, so the
# zen-serve `fdb` feature builds and runs without extra environment.
# Otherwise set: LIBRARY_PATH=DEST/usr/lib LD_LIBRARY_PATH=DEST/usr/lib
set -eu
VERSION=7.3.79
CLIENTS_SHA256=52cc22565c42e7eb60c08f395a0626483735c311be0d80b7c035ac8e328e2fff
SERVER_SHA256=f85a4126a76919a4dd6194e69bc29200d0d4fa3d7b5261e7eb91c090b7fdba55

usage() {
  sed -n '2,/^set -eu/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2
  exit 2
}

DEST=/opt/foundationdb
backup=1 dr=1 fdbmonitor=1 dedupe=auto
for arg in "$@"; do
  case "$arg" in
    --no-backup) backup=0 ;;
    --no-dr) dr=0 ;;
    --no-fdbmonitor) fdbmonitor=0 ;;
    --dedupe=auto|--dedupe=reflink|--dedupe=hard|--dedupe=soft|--dedupe=none)
      dedupe=${arg#--dedupe=} ;;
    -h|--help) usage ;;
    -*) echo "install-fdb.sh: unknown option $arg" >&2; usage ;;
    *) DEST=$arg ;;
  esac
done

case "$(uname -m)" in
  x86_64) ;;
  *) echo "install-fdb.sh: only x86_64 Linux packages are pinned" >&2; exit 1 ;;
esac

base=https://github.com/apple/foundationdb/releases/download/$VERSION
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fetch() { # name sha256
  curl -fsSL --retry 4 -o "$tmp/$1" "$base/$1"
  echo "$2  $tmp/$1" | sha256sum -c - >/dev/null || {
    echo "install-fdb.sh: sha256 mismatch for $1" >&2
    exit 1
  }
}
fetch "foundationdb-clients_${VERSION}-1_amd64.deb" "$CLIENTS_SHA256"
fetch "foundationdb-server_${VERSION}-1_amd64.deb" "$SERVER_SHA256"
mkdir -p "$DEST"
for deb in "$tmp"/*.deb; do
  dpkg-deb -x "$deb" "$DEST"
done

# Opt-outs (nothing is skipped by default).
[ "$backup" = 1 ] || rm -f "$DEST/usr/bin/fdbbackup" "$DEST/usr/bin/fdbrestore" \
  "$DEST/usr/lib/foundationdb/backup_agent/backup_agent" &&
  rmdir "$DEST/usr/lib/foundationdb/backup_agent" 2>/dev/null || true
[ "$dr" = 1 ] || rm -f "$DEST/usr/bin/fdbdr" "$DEST/usr/bin/dr_agent"
[ "$fdbmonitor" = 1 ] || rm -f "$DEST/usr/lib/foundationdb/fdbmonitor"

# Store the remaining copies of the backup/DR program once.
# link_to SRC DST MODE: replace DST (a copy of SRC) by a link of kind MODE.
link_to() {
  case "$3" in
    reflink) cp --reflink=always -f "$1" "$2.zen-tmp" 2>/dev/null && mv -f "$2.zen-tmp" "$2" ;;
    hard) ln -f "$1" "$2.zen-tmp" 2>/dev/null && mv -f "$2.zen-tmp" "$2" ;;
    soft) ln -sf "$(realpath --relative-to="$(dirname "$2")" "$1")" "$2.zen-tmp" 2>/dev/null &&
      mv -f "$2.zen-tmp" "$2" ;;
  esac
}
saved=0
if [ "$dedupe" != none ]; then
  src=
  for f in usr/bin/fdbbackup usr/bin/fdbrestore usr/bin/fdbdr usr/bin/dr_agent \
    usr/lib/foundationdb/backup_agent/backup_agent; do
    [ -f "$DEST/$f" ] || continue
    if [ -z "$src" ]; then src=$DEST/$f; continue; fi
    cmp -s "$src" "$DEST/$f" || continue # only identical files
    modes=$dedupe
    [ "$dedupe" = auto ] && modes="reflink hard soft"
    for m in $modes; do
      if link_to "$src" "$DEST/$f" "$m"; then
        saved=$((saved + $(stat -c %s "$src")))
        echo "  $f: $m"
        break
      fi
      rm -f "$DEST/$f.zen-tmp"
    done
  done
fi

if [ "$(id -u)" = 0 ]; then
  ln -sf "$DEST/usr/lib/libfdb_c.so" /usr/lib/libfdb_c.so
  ldconfig 2>/dev/null || true
fi
echo "FoundationDB $VERSION installed in $DEST ($(du -sh "$DEST" | cut -f1); dedupe=$dedupe saved ~$((saved / 1048576)) MB)"
"$DEST/usr/sbin/fdbserver" --version | head -1
