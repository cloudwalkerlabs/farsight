#!/bin/bash
# M2 resize and scale run on one machine, as tools/m1/e2e.sh: a headless
# labwc stands in for the client's desktop, farsight-desktop runs in it with
# a terminal in the remote session.
#
#   tools/m2/resize.sh OUTDIR
#
# 1. drag-resize: the client window grows by 100 px five times, 30 ms apart
#    (labwc's ResizeRelative, bound to F9), then shrinks back (F10);
# 2. a move to a different-DPI monitor: the client's output goes to scale
#    1.5, then 2, then back to 1 (wlr-randr).
# Screenshots of the client's desktop after each step go in OUTDIR, and the
# server's "layout applied" lines say how long each change held frames.
#
# Needs what tools/m1/e2e.sh needs, plus wlr-randr. SERVER_ARGS and
# CLIENT_ARGS are passed on.
set -e
OUT=$(mkdir -p "$1" && cd "$1" && pwd)
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=$ROOT/target/release
WLTOOL=$ROOT/tools/m1/wltool/target/release/wltool
PORT=7798
export NO_COLOR=1

# The client's desktop binds F9/F10 to resize the focused window.
mkdir -p "$OUT/outer" "$OUT/inner"
cat > "$OUT/outer/rc.xml" <<'XML'
<?xml version="1.0"?>
<labwc_config>
  <keyboard>
    <keybind key="F9"><action name="ResizeRelative" right="100" /></keybind>
    <keybind key="F10"><action name="ResizeRelative" right="-100" /></keybind>
  </keyboard>
</labwc_config>
XML
printf '<?xml version="1.0"?>\n<labwc_config/>\n' > "$OUT/inner/rc.xml"

pids=()
cleanup() {
	exec 7>&- 2>/dev/null || true
	for p in "${pids[@]}"; do kill -- "-$p" 2>/dev/null || true; done
}
trap cleanup EXIT

before=$(ls "$XDG_RUNTIME_DIR")
env -u WAYLAND_DISPLAY -u DISPLAY WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 \
	WLR_RENDERER=gles2 WLR_RENDER_DRM_DEVICE=${RENDER_NODE:-/dev/dri/renderD128} \
	setsid labwc -C "$OUT/outer" > "$OUT/outer.log" 2>&1 < /dev/null &
pids+=($!)
for _ in $(seq 50); do
	OUTER=$(comm -13 <(echo "$before") <(ls "$XDG_RUNTIME_DIR") | grep -m1 '^wayland-[0-9]*$' || true)
	[ -n "$OUTER" ] && break
	sleep 0.1
done
[ -n "$OUTER" ] || { echo "the headless labwc did not start" >&2; exit 1; }
randr() { WAYLAND_DISPLAY=$OUTER wlr-randr --output HEADLESS-1 "$@"; }
randr --custom-mode 2560x1600

rm -f "$OUT/ctl"; mkfifo "$OUT/ctl"
env -u DISPLAY RUST_LOG=info,smithay=warn setsid "$BIN/farsight-server" --port $PORT $SERVER_ARGS \
	--identity "$OUT/identity" -- labwc -C "$OUT/inner" -s \
	"alacritty -o window.dimensions.columns=100 -o window.dimensions.lines=30 -e sh -c 'ls -l /usr/bin | head -40; sleep 1000'" \
	> "$OUT/server.log" 2>&1 < "$OUT/ctl" &
pids+=($!)
exec 7>"$OUT/ctl"
sleep 2
env -u DISPLAY WAYLAND_DISPLAY=$OUTER setsid "$BIN/farsight-desktop" 127.0.0.1:$PORT --size 1280x800 $CLIENT_ARGS \
	> "$OUT/client.log" 2>&1 < /dev/null &
pids+=($!)
sleep 3

wl() { WAYLAND_DISPLAY=$OUTER "$WLTOOL" "$@"; }
shot() {
	wl shot "$OUT/$1.ppm" > /dev/null
	python3 -c "import sys; from PIL import Image; Image.open(sys.argv[1]).save(sys.argv[2])" "$OUT/$1.ppm" "$OUT/$1.png"
	rm "$OUT/$1.ppm"
	echo "$1: $(grep -c 'layout applied' "$OUT/server.log") layouts applied so far"
}
# Focus the client window.
wl move 1280 800; wl click; sleep 0.5
shot 1-start

for _ in 1 2 3 4 5; do wl key 67; sleep 0.03; done
sleep 1.5
shot 2-wider
for _ in 1 2 3 4 5; do wl key 68; sleep 0.03; done
sleep 1.5
shot 3-back

randr --scale 1.5; sleep 2
shot 4-scale1.5
randr --scale 2; sleep 2
shot 5-scale2
randr --scale 1; sleep 2
shot 6-scale1

echo quit >&7
sleep 1
echo "client SetLayout:"; grep -o 'SetLayout layout=.*' "$OUT/client.log" | sed 's/^/  /'
echo "server:"; grep -E 'layout applied|timed out|nested compositor' "$OUT/server.log" | sed 's/.*INFO //; s/^/  /'
