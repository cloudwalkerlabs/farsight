#!/bin/bash
# M1 end-to-end run on one machine, with no display needed: a headless labwc
# stands in for the client's desktop, farsight-desktop runs in it, and
# farsight-server serves its own nested session on 127.0.0.1.
#
#   tools/m1/e2e.sh OUTDIR [SCENARIO]
#
# SCENARIO:
#   gears      (default) es2gears in the session for 20 s; the client logs
#              its latency every 5 s
#   input      types "echo farsight" into a terminal in the session through
#              the client window, then screenshots the client's desktop
#   keyframes  a screen full of text, video paced at 20 Mbit/s and a forced
#              keyframe every 100 ms for 10 s; watch rtt_max_ms
#
# Needs: cargo build --release -p farsight-server -p farsight-desktop
#        (cd tools/m1/wltool && cargo build --release)
#        labwc, es2gears_wayland, alacritty, python3 with Pillow (input only)
set -e
OUT=$(mkdir -p "$1" && cd "$1" && pwd); SCENARIO=${2:-gears}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=$ROOT/target/release
WLTOOL=$ROOT/tools/m1/wltool/target/release/wltool
PORT=7799
export NO_COLOR=1

mkdir -p "$OUT/cfg"
printf '<?xml version="1.0"?>\n<labwc_config/>\n' > "$OUT/cfg/rc.xml"

pids=()
cleanup() {
	exec 7>&- 2>/dev/null || true
	for p in "${pids[@]}"; do kill -- "-$p" 2>/dev/null || true; done
}
trap cleanup EXIT

# The client's desktop: a new headless labwc on the next free socket.
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
WAYLAND_DISPLAY=$OUTER wlr-randr --output HEADLESS-1 --custom-mode 1920x1080 2>/dev/null || true

case $SCENARIO in
	gears) APP=es2gears_wayland; RATE=100 ;;
	input) APP=alacritty; RATE=100 ;;
	keyframes)
		APP="alacritty -o window.dimensions.columns=200 -o window.dimensions.lines=55 -e sh -c 'find /usr/lib | head -5000; sleep 1000'"
		RATE=20 ;;
	*) echo "unknown scenario $SCENARIO" >&2; exit 1 ;;
esac

rm -f "$OUT/ctl"; mkfifo "$OUT/ctl"
env -u DISPLAY RUST_LOG=info,smithay=warn setsid "$BIN/farsight-server" --port $PORT \
	--identity "$OUT/identity" --rate $RATE -- labwc -C "$OUT/cfg" -s "$APP" \
	> "$OUT/server.log" 2>&1 < "$OUT/ctl" &
pids+=($!)
exec 7>"$OUT/ctl"
sleep 2
env -u DISPLAY WAYLAND_DISPLAY=$OUTER setsid "$BIN/farsight-desktop" 127.0.0.1:$PORT --size 1600x900 \
	> "$OUT/client.log" 2>&1 < /dev/null &
pids+=($!)
sleep 3

wl() { WAYLAND_DISPLAY=$OUTER "$WLTOOL" "$@"; }
case $SCENARIO in
	gears) sleep 20 ;;
	input)
		# The client window is centred: point at the middle of the terminal.
		wl move 900 500; wl move 960 540; wl click; sleep 0.3
		wl key 18 46 35 24 57 33 30 19 31 23 34 35 20 28  # echo farsight⏎
		sleep 1
		wl shot "$OUT/client.ppm"
		python3 -c "import sys; from PIL import Image; Image.open(sys.argv[1]).save(sys.argv[2])" \
			"$OUT/client.ppm" "$OUT/client.png"
		echo "screenshot: $OUT/client.png" ;;
	keyframes)
		for _ in $(seq 100); do echo keyframe >&7; sleep 0.1; done
		sleep 6 ;;
esac
echo quit >&7
sleep 1
grep 'latency ms' "$OUT/client.log" | sed 's/.*stats: //' || true
