#!/bin/sh
# Print the Homebrew formula for a macOS release tarball, from
# farsight-desktop.rb.in.
#
#   formula.sh VERSION URL TARBALL > farsight-desktop.rb
#
# URL is where Homebrew downloads the tarball from (the release's asset, or
# a file:// URL to test a build), and TARBALL that file here, for its
# checksum.
set -eu

if [ $# -ne 3 ]; then
  echo "usage: $0 VERSION URL TARBALL" >&2
  exit 2
fi
version=$1
url=$2
tarball=$3

sha256=$(shasum -a 256 "$tarball" 2>/dev/null || sha256sum "$tarball")
sed -e "s|@VERSION@|$version|" \
  -e "s|@URL@|$url|" \
  -e "s|@SHA256@|${sha256%% *}|" \
  "$(dirname "$0")/farsight-desktop.rb.in"
