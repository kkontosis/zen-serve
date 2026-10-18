#!/bin/sh
# Build OpenSSH's ssh and ssh-keygen from source into <dir>, for the SSH
# engine's tests of the hybrid key exchange mlkem768x25519-sha256
# (crates/zen-svc/tests/openssh_pq.rs), which OpenSSH has from 9.9 on;
# Ubuntu 24.04 ships 9.6. The release is pinned by tag and commit.
#
#   scripts/build-openssh.sh <dir>
#   ZEN_TEST_SSH=<dir>/ssh cargo test -p zen-svc --test openssh_pq -- --ignored
#
# Needs git, a C compiler, autoconf, and OpenSSL's and zlib's headers:
#   apt install build-essential autoconf libssl-dev zlib1g-dev
# Nothing is installed: the two programs are copied into <dir>.
set -eu
dir=${1:?usage: scripts/build-openssh.sh <dir>}
tag=V_10_6_P1
commit=a8c72b842423f0774f542d3c2197247c3dd28d74
version=OpenSSH_10.6p1

if [ -x "$dir/ssh" ] && [ -x "$dir/ssh-keygen" ] && "$dir/ssh" -V 2>&1 | grep -q "^$version,"; then
  echo "$version is in $dir"
  exit 0
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git clone -q --depth 1 --branch "$tag" https://github.com/openssh/openssh-portable "$work/src"
got=$(git -C "$work/src" rev-parse HEAD)
if [ "$got" != "$commit" ]; then
  echo "$tag is $got, expected $commit" >&2
  exit 1
fi
cd "$work/src"
autoreconf -i >/dev/null
./configure -q --without-pam --without-selinux >/dev/null
make -s -j"$(nproc)" ssh ssh-keygen >/dev/null
mkdir -p "$dir"
cp ssh ssh-keygen "$dir/"
"$dir/ssh" -V
