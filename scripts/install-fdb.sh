#!/bin/sh
# Install the pinned FoundationDB release (client library + server binaries)
# from Apple's GitHub releases, verified by sha256.
#
#   scripts/install-fdb.sh [DEST]      (default DEST: /opt/foundationdb)
#
# The .deb packages are unpacked into DEST (no services are started):
#   DEST/usr/sbin/fdbserver, DEST/usr/bin/{fdbcli,fdbbackup,fdbrestore,backup_agent},
#   DEST/usr/lib/libfdb_c.so
# When run as root, libfdb_c.so is also linked into /usr/lib, so the
# zen-serve `fdb` feature builds and runs without extra environment.
# Otherwise set: LIBRARY_PATH=DEST/usr/lib LD_LIBRARY_PATH=DEST/usr/lib
set -eu

VERSION=7.3.79
CLIENTS_SHA256=52cc22565c42e7eb60c08f395a0626483735c311be0d80b7c035ac8e328e2fff
SERVER_SHA256=f85a4126a76919a4dd6194e69bc29200d0d4fa3d7b5261e7eb91c090b7fdba55

DEST=${1:-/opt/foundationdb}
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

if [ "$(id -u)" = 0 ]; then
  ln -sf "$DEST/usr/lib/libfdb_c.so" /usr/lib/libfdb_c.so
  ldconfig 2>/dev/null || true
fi

echo "FoundationDB $VERSION installed in $DEST"
"$DEST/usr/sbin/fdbserver" --version | head -1
