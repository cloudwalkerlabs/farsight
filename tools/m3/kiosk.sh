#!/bin/bash
# M3 kiosk mode on one machine, as tools/m1/e2e.sh: one app (mousepad by
# default; KIOSK_APP), no nested compositor. Screenshots in OUTDIR:
#
#   1-start      the app fills the client window
#   2-menu       a click on its first menu opens a popup
#   3-closed     a click elsewhere closes it
#   4-scale2     the client's output at scale 2: the app redraws sharply
#
#   tools/m3/kiosk.sh OUTDIR
set -e
OUT=$(mkdir -p "$1" && cd "$1" && pwd)
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=$ROOT/target/release
WLTOOL=$ROOT/tools/m1/wltool/target/release/wltool
PORT=7794
APP=${KIOSK_APP:-mousepad}
export NO_COLOR=1

mkdir -p "$OUT/cfg"
printf '<?xml version="1.0"?>\n<labwc_config/>\n' > "$OUT/cfg/rc.xml"
pids=()
cleanup() {
	for p in "${pids[@]}"; do kill -- "-$p" 2>/dev/null || true; done
}
trap cleanup EXIT

before=$(ls "$XDG_RUNTIME_DIR")
env -u WAYLAND_DISPLAY -u DISPLAY WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 \
	WLR_RENDERER=gles2 WLR_RENDER_DRM_DEVICE=${RENDER_NODE:-/dev/dri/renderD128} \
	setsid labwc -C "$OUT/cfg" > "$OUT/outer.log" 2>&1 < /dev/null &
pids+=($!)
for _ in $(seq 50); do
	OUTER=$(comm -13 <(echo "$before") <(ls "$XDG_RUNTIME_DIR") | grep -m1 '^wayland-[0-9]*$' || true)
	[ -n "$OUTER" ] && break
	sleep 0.1
done
[ -n "$OUTER" ] || { echo "the headless labwc did not start" >&2; exit 1; }
outer() { timeout 10 env WAYLAND_DISPLAY=$OUTER "$WLTOOL" "$@"; }
outer_randr() { WAYLAND_DISPLAY=$OUTER wlr-randr --output HEADLESS-1 "$@"; }
outer_randr --custom-mode 2560x1600
shot() {
	outer shot "$OUT/$1.ppm" > /dev/null
	python3 -c "import sys; from PIL import Image; Image.open(sys.argv[1]).save(sys.argv[2])" "$OUT/$1.ppm" "$OUT/$1.png"
	rm "$OUT/$1.ppm"
}

mkdir -p "$OUT/server"; rm -f "$OUT/client/known_hosts"
"$BIN/farsight-desktop" --config-dir "$OUT/client" --print-key > "$OUT/server/authorized_keys"
env -u DISPLAY RUST_LOG=info,smithay=warn setsid "$BIN/farsight-server" --port $PORT --no-audio --app \
	--config-dir "$OUT/server" -- $APP > "$OUT/server.log" 2>&1 < /dev/null &
pids+=($!)
sleep 2
env -u DISPLAY WAYLAND_DISPLAY=$OUTER setsid "$BIN/farsight-desktop" --no-audio --config-dir "$OUT/client" \
	--size 1280x800 127.0.0.1:$PORT > "$OUT/client.log" 2>&1 < /dev/null &
pids+=($!)
sleep 4
shot 1-start
# The client window is centred on the 2560×1600 output: its content starts
# at about (640, 427). The first menu is at the app's top left.
outer move 660 427; outer click; sleep 1
shot 2-menu
outer move 1280 1000; outer click; sleep 1
shot 3-closed
outer_randr --scale 2; sleep 3
shot 4-scale2
grep -E "window created|layout applied|timed out|popup" "$OUT/server.log" | sed 's/.*INFO //; s/^/  /'
