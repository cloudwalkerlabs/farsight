#!/bin/sh
# Check an installed farsight package, as the .deb or the Arch package
# installs it: the command runs, and what it loads or starts at runtime,
# which the package's dependencies must bring, is there.
#
#   check_installed.sh PACKAGE
#
# PACKAGE is farsight-server or farsight-desktop.
set -eu

if [ $# -ne 1 ]; then
  echo "usage: $0 PACKAGE" >&2
  exit 2
fi
package=$1

missing=0
need_lib() {
  for lib; do
    if ! ldconfig -p | grep -q "^[[:space:]]*$lib "; then
      echo "check_installed.sh: $lib isn't installed" >&2
      missing=1
    fi
  done
}

"$package" --help >/dev/null
test -s "/usr/share/doc/$package/copyright"
test -s "/usr/share/doc/$package/FFMPEG.txt"
case $package in
  farsight-server)
    unit=/usr/lib/systemd/system/farsight-server@.service
    grep -q '^ExecStart=/usr/bin/farsight-server ' "$unit"
    need_lib libEGL.so.1
    for program in dbus-daemon pipewire wireplumber pipewire-pulse; do
      if ! command -v "$program" >/dev/null; then
        echo "check_installed.sh: $program isn't installed" >&2
        missing=1
      fi
    done
    test -f /usr/share/X11/xkb/rules/evdev
    ;;
  farsight-desktop)
    need_lib libEGL.so.1 libwayland-client.so.0 libwayland-egl.so.1 libxkbcommon.so.0
    ;;
  *)
    echo "check_installed.sh: PACKAGE is farsight-server or farsight-desktop, not $package" >&2
    exit 2
    ;;
esac
# Some driver for EGL: Mesa's software one, at least.
if ! ls /usr/lib/dri/*_dri.so /usr/lib/*/dri/*_dri.so 2>/dev/null | grep -q .; then
  echo "check_installed.sh: no Mesa DRI drivers" >&2
  missing=1
fi
exit $missing
