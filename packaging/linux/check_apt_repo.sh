#!/bin/sh
# Install farsight from the apt repository apt_repo.sh built, on a clean
# Debian or Ubuntu, as README.md tells users to, except that the source
# points at REPO_DIR instead of GitHub Pages. Both packages must install
# and pass check_installed.sh.
#
#   check_apt_repo.sh REPO_DIR
#
# Runs as root, in a throwaway container: it changes the system's apt
# sources. apt checks the signatures against farsight.gpg alone.
set -eu

if [ $# -ne 1 ]; then
  echo "usage: $0 REPO_DIR" >&2
  exit 2
fi
packaging=$(cd "$(dirname "$0")" && pwd)
# apt downloads as the _apt user, which must be able to read it.
repo=/srv/farsight-apt
rm -rf "$repo"
cp -R "$1" "$repo"
chmod -R a+rX "$repo"

export DEBIAN_FRONTEND=noninteractive
# Ubuntu's container images leave out /usr/share/doc, which a full system
# has.
rm -f /etc/dpkg/dpkg.cfg.d/excludes
install -d -m 755 /etc/apt/keyrings
install -m 644 "$repo/farsight.gpg" /etc/apt/keyrings/farsight.gpg
sed "s|^URIs: .*|URIs: file:$repo|" "$repo/farsight.sources" \
  >/etc/apt/sources.list.d/farsight.sources
apt-get update
apt-get install -y --no-install-recommends farsight-server farsight-desktop
for package in farsight-server farsight-desktop; do
  "$packaging/check_installed.sh" "$package"
done
