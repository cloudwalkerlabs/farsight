#!/bin/sh
# Print the Arch Linux PKGBUILD for a release's package, from
# PACKAGE-bin.PKGBUILD.in.
#
#   pkgbuild.sh PACKAGE TAG AMD64_DEB ARM64_DEB > PACKAGE-bin.PKGBUILD
#
# PACKAGE is farsight-server or farsight-desktop, TAG the release's tag (the
# .debs are downloaded from it), and the .debs the release's packages,
# named PACKAGE_<version>_<arch>.deb.
set -eu

if [ $# -ne 4 ]; then
  echo "usage: $0 PACKAGE TAG AMD64_DEB ARM64_DEB" >&2
  exit 2
fi
package=$1
tag=$2
amd64=$3
arm64=$4

template=$(dirname "$0")/$package-bin.PKGBUILD.in
if [ ! -f "$template" ]; then
  echo "pkgbuild.sh: PACKAGE is farsight-server or farsight-desktop, not $package" >&2
  exit 2
fi
debver=$(basename "$amd64" | sed -n "s/^${package}_\(.*\)_amd64\.deb\$/\1/p")
if [ -z "$debver" ] || [ "$(basename "$arm64")" != "${package}_${debver}_arm64.deb" ]; then
  echo "pkgbuild.sh: expected ${package}_<version>_amd64.deb and _arm64.deb of the same version" >&2
  exit 1
fi
# pkgver can't contain "-", ":" or "/". Release versions are
# MAJOR.MINOR.PATCH; a dev build's 0.0.0~dev+abc1234 becomes 0.0.0.dev_abc1234.
pkgver=$(printf '%s' "$debver" | tr '~+' '._')
case $pkgver in
  *[-:/]*)
    echo "pkgbuild.sh: $pkgver isn't a valid pkgver" >&2
    exit 1
    ;;
esac

sed -e "s|@PKGVER@|$pkgver|" \
  -e "s|@DEBVER@|$debver|g" \
  -e "s|@TAG@|$tag|" \
  -e "s|@SHA256_AMD64@|$(sha256sum "$amd64" | cut -d' ' -f1)|" \
  -e "s|@SHA256_ARM64@|$(sha256sum "$arm64" | cut -d' ' -f1)|" \
  "$template"
