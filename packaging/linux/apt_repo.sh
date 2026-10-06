#!/bin/sh
# Build farsight's signed apt repository from a release's .debs.
#
#   GNUPGHOME=... apt_repo.sh OUT_DIR DEB...
#
# GNUPGHOME signs with the release key (packaging/release_key.sh). OUT_DIR
# becomes a flat "stable" suite with one component, main, holding only
# these packages, which GitHub Pages serves at $URL:
#
#   OUT_DIR/pool/main/<package>_<version>_<arch>.deb
#   OUT_DIR/dists/stable/{InRelease,Release,Release.gpg}
#   OUT_DIR/dists/stable/main/binary-<arch>/Packages{,.gz}
#   OUT_DIR/farsight.gpg      the release key, for /etc/apt/keyrings
#   OUT_DIR/farsight.sources  the source, for /etc/apt/sources.list.d
#
# Needs apt-ftparchive (apt-utils), dpkg-deb and gpg. check_apt_repo.sh
# installs from the result.
set -eu

URL=https://cloudwalkerlabs.github.io/farsight/apt

if [ $# -lt 2 ]; then
  echo "usage: $0 OUT_DIR DEB..." >&2
  exit 2
fi
out=$1
shift
key=$(cd "$(dirname "$0")/.." && pwd)/release-key.asc

rm -rf "$out"
mkdir -p "$out/pool/main"
cp "$@" "$out/pool/main/"
cd "$out"

archs=$(for deb in pool/main/*.deb; do dpkg-deb --field "$deb" Architecture; done | sort -u | tr '\n' ' ')
archs=${archs% }
for arch in $archs; do
  dir=dists/stable/main/binary-$arch
  mkdir -p "$dir"
  apt-ftparchive --arch "$arch" packages pool >"$dir/Packages"
  gzip -9nk "$dir/Packages"
done
apt-ftparchive \
  -o APT::FTPArchive::Release::Origin=farsight \
  -o APT::FTPArchive::Release::Label=farsight \
  -o APT::FTPArchive::Release::Suite=stable \
  -o APT::FTPArchive::Release::Codename=stable \
  -o "APT::FTPArchive::Release::Architectures=$archs" \
  -o APT::FTPArchive::Release::Components=main \
  -o "APT::FTPArchive::Release::Description=farsight, a low-latency remote desktop" \
  release dists/stable >Release.tmp
mv Release.tmp dists/stable/Release
gpg --clearsign --output dists/stable/InRelease dists/stable/Release
gpg --detach-sign --armor --output dists/stable/Release.gpg dists/stable/Release

gpg --dearmor <"$key" >farsight.gpg
# The signatures must check against the published key alone.
gpgv --keyring "$PWD/farsight.gpg" dists/stable/InRelease
gpgv --keyring "$PWD/farsight.gpg" dists/stable/Release.gpg dists/stable/Release

cat >farsight.sources <<SOURCES
Types: deb
URIs: $URL
Suites: stable
Components: main
Architectures: $archs
Signed-By: /etc/apt/keyrings/farsight.gpg
SOURCES
