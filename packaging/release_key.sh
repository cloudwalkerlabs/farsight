#!/bin/sh
# Set up a GnuPG home that signs with Cloudwalker Labs' release key, from the
# build workflow's secrets: the cloudwalkerlabs organization's, unless this
# repository sets its own of the same names.
#
#   RELEASE_GPG_KEY=... RELEASE_GPG_PASSPHRASE=... release_key.sh DIR
#   GNUPGHOME=DIR gpg --detach-sign --armor FILE
#
# RELEASE_GPG_KEY is the key's signing subkey (gpg --export-secret-subkeys:
# the primary key stays offline), RELEASE_GPG_PASSPHRASE its passphrase.
# The key must be the one whose public half is packaging/release-key.asc,
# which users verify with, and must be able to sign; the script fails
# otherwise. DIR's gpg.conf then signs with it without asking.
set -eu

if [ $# -ne 1 ]; then
  echo "usage: $0 DIR" >&2
  exit 2
fi
home=$1
public=$(dirname "$0")/release-key.asc

: "${RELEASE_GPG_KEY:?is not set}"
: "${RELEASE_GPG_PASSPHRASE:?is not set}"

mkdir -p "$home"
chmod 700 "$home"
export GNUPGHOME="$home"
fpr=$(gpg --show-keys --with-colons "$public" | awk -F: '/^fpr/ { print $10; exit }')
printf '%s' "$RELEASE_GPG_PASSPHRASE" >"$home/passphrase"
chmod 600 "$home/passphrase"
printf '%s\n' "$RELEASE_GPG_KEY" | gpg --batch --quiet --import
cat >"$home/gpg.conf" <<CONF
batch
pinentry-mode loopback
passphrase-file $home/passphrase
local-user $fpr
CONF

# A usable secret signing key under the published primary key: "ssb" with
# "s" among its capabilities, and not a stub ("#" in the serial field).
if ! gpg --list-secret-keys --with-colons "$fpr" >"$home/keys" 2>/dev/null; then
  echo "release_key.sh: RELEASE_GPG_KEY has no secret key under $fpr ($public): another key, or no subkeys" >&2
  exit 1
fi
if ! awk -F: '$1 == "ssb" && $12 ~ /s/ && $15 !~ /^#/ { found = 1 } END { exit !found }' "$home/keys"; then
  echo "release_key.sh: RELEASE_GPG_KEY has no signing subkey for $fpr" >&2
  exit 1
fi
# Proves the passphrase too.
echo test | gpg --detach-sign --armor >/dev/null
