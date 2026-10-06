#!/bin/sh
# Install a .deb on this (clean, Debian or Ubuntu) system, with its Depends
# but not its Recommends, and check it (check_installed.sh).
#
#   check_deb.sh DEB
#
# Needs root (apt). Meant for a throwaway container, as the build workflow
# runs it: one per package, so neither gets the other's dependencies.
set -eu

if [ $# -ne 1 ]; then
  echo "usage: $0 DEB" >&2
  exit 2
fi
deb=$(realpath "$1")

export DEBIAN_FRONTEND=noninteractive
# Ubuntu's container images leave out /usr/share/doc, which a full system
# has.
rm -f /etc/dpkg/dpkg.cfg.d/excludes
apt-get update -qq
apt-get install -y -qq --no-install-recommends "$deb" >/dev/null
dpkg-deb --field "$deb" Depends Recommends
"$(dirname "$0")/check_installed.sh" "$(dpkg-deb --field "$deb" Package)"
echo "check_deb.sh: $deb installs and runs"
