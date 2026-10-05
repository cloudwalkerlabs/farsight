#!/bin/sh
# Pack a release binary into a .deb: farsight-server or farsight-desktop.
#
#   build_deb.sh PACKAGE BINARY NOTICES VERSION OUT_DIR
#
# PACKAGE is the package and the command's name, BINARY cargo's binary,
# installed as /usr/bin/PACKAGE, and NOTICES tools/ffmpeg/build.sh's
# PREFIX/notice, installed with the copyright file in /usr/share/doc/PACKAGE.
# The server also gets dist/farsight-server@.service, as a system unit.
# Runs on Debian or Ubuntu: it needs dpkg-deb, and dpkg-shlibdeps to work
# out the dependencies from the ELF file. Build on the oldest release you
# want to support, since the glibc it links against is the oldest one the
# package will install on.
set -eu

if [ $# -ne 5 ]; then
  echo "usage: $0 PACKAGE BINARY NOTICES VERSION OUT_DIR" >&2
  exit 2
fi
package=$1
binary=$2
notices=$3
version=$4
out_dir=$5

repo=$(cd "$(dirname "$0")/../.." && pwd)
arch=$(dpkg --print-architecture)

# Loaded at runtime (dlopen), so dpkg-shlibdeps can't see them. EGL needs
# Mesa's drivers to do anything. VA-API is recommended only: without it the
# server sends tiles and the client decodes in software.
va="libva-drm2, libdrm2, mesa-va-drivers | va-driver"
case $package in
  farsight-server)
    # The session's bus and audio daemons, and the XKB keymaps; labwc is
    # the default desktop, but any nested compositor will do (README.md),
    # and Debian 12 has none.
    depends="libegl1, libgl1-mesa-dri, dbus-daemon | dbus, pipewire, wireplumber, pipewire-pulse, xkb-data"
    recommends="labwc, $va"
    suggests="sway"
    summary="Low-latency remote desktop server for headless Linux machines"
    description=" farsight-server runs a persistent Wayland desktop session, isolated
 from anything else on the machine, and streams it to farsight clients,
 with its sound and the client's microphone. It encodes with VA-API or
 NVENC, and sends sharp tiles without either.
 .
 The farsight-server@.service unit keeps a session running; it explains
 how to set it up."
    ;;
  farsight-desktop)
    # winit and glutin, on Wayland.
    depends="libegl1, libgl1-mesa-dri, libwayland-client0, libwayland-egl1, libxkbcommon0"
    recommends=$va
    suggests=""
    summary="Low-latency remote desktop client for farsight servers"
    description=" farsight-desktop connects to a farsight server from a Wayland
 desktop. The remote desktop follows the window's size and scale, its
 sound plays here, and the clipboard and the microphone are shared.
 Video decodes with VA-API, or in software."
    ;;
  *)
    echo "build_deb.sh: PACKAGE is farsight-server or farsight-desktop, not $package" >&2
    exit 2
    ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
root=$work/root
doc=$root/usr/share/doc/$package

mkdir -p "$root/usr/bin" "$doc" "$root/DEBIAN"
# Unstripped, as in the tarball: the release profile keeps line tables for
# backtraces.
install -m 755 "$binary" "$root/usr/bin/$package"
if [ "$package" = farsight-server ]; then
  unit=$root/usr/lib/systemd/system/farsight-server@.service
  mkdir -p "$(dirname "$unit")"
  sed 's|^ExecStart=/usr/local/bin/|ExecStart=/usr/bin/|' \
    "$repo/dist/farsight-server@.service" >"$unit"
  grep -q '^ExecStart=/usr/bin/farsight-server ' "$unit"
  chmod 644 "$unit"
fi
install -m 644 "$repo/README.md" "$notices"/* "$doc/"
# Debian's machine-readable format. The repository has no license text of
# its own yet, so this names the licenses Cargo.toml declares.
cat >"$doc/copyright" <<COPYRIGHT
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: farsight
Source: https://github.com/cloudwalkerlabs/farsight

Files: *
Copyright: farsight's authors
License: MIT or Apache-2.0
 farsight is licensed under the MIT license or the Apache License 2.0, at
 your option. On Debian systems, the Apache License 2.0 is in
 /usr/share/common-licenses/Apache-2.0.
 .
 $package links FFmpeg (LGPL-2.1), dav1d (BSD-2-clause) and libxkbcommon
 (MIT) statically: FFMPEG.txt and the COPYING files next to this one have
 their licenses and FFmpeg's source.
COPYRIGHT
chmod -R u=rwX,go=rX "$root/usr"

# dpkg-shlibdeps wants to run from a source tree; give it a minimal one.
mkdir -p "$work/src/debian"
printf 'Source: farsight\n\nPackage: %s\nArchitecture: any\n' "$package" \
  >"$work/src/debian/control"
shlibs=$(
  cd "$work/src" &&
    dpkg-shlibdeps -O "$root/usr/bin/$package" 2>"$work/shlibdeps.log" |
    sed -n 's/^shlibs:Depends=//p'
) || {
  cat "$work/shlibdeps.log" >&2
  exit 1
}
test -n "$shlibs"

{
  printf 'Package: %s\n' "$package"
  printf 'Version: %s\n' "$version"
  printf 'Architecture: %s\n' "$arch"
  printf 'Maintainer: fanchao <dev@fanchao.dev>\n'
  printf 'Installed-Size: %s\n' "$(du -sk --exclude=DEBIAN "$root" | cut -f1)"
  printf 'Depends: %s, %s\n' "$shlibs" "$depends"
  printf 'Recommends: %s\n' "$recommends"
  if [ -n "$suggests" ]; then printf 'Suggests: %s\n' "$suggests"; fi
  printf 'Section: net\nPriority: optional\n'
  printf 'Homepage: https://github.com/cloudwalkerlabs/farsight\n'
  printf 'Description: %s\n%s\n' "$summary" "$description"
} >"$root/DEBIAN/control"

mkdir -p "$out_dir"
deb=$out_dir/${package}_${version}_$arch.deb
dpkg-deb --root-owner-group --build "$root" "$deb" >/dev/null
echo "$deb"
