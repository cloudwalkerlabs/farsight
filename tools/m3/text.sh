#!/bin/bash
# M3 clipboard and text input on one machine, as tools/m1/e2e.sh.
#
#   tools/m3/text.sh OUTDIR
#
# 1. text copied on the client's desktop is pasted by an app in the
#    session (fetched from the client only then);
# 2. text copied in the session is pasted on the client's desktop;
# 3. text goes into a terminal in the session through input-method-v2:
#    once from the server's stdin, once from an input method on the
#    client's desktop (wltool commit), through the client window's
#    text-input. OUTDIR/text.png shows the terminal.
#
# Needs what tools/m1/e2e.sh needs.
set -e
OUT=$(mkdir -p "$1" && cd "$1" && pwd)
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=$ROOT/target/release
WLTOOL=$ROOT/tools/m1/wltool/target/release/wltool
PORT=7795
export NO_COLOR=1

mkdir -p "$OUT/cfg"
printf '<?xml version="1.0"?>\n<labwc_config/>\n' > "$OUT/cfg/rc.xml"
pids=()
cleanup() {
	exec 7>&- 2>/dev/null || true
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

mkdir -p "$OUT/server"; rm -f "$OUT/client/known_hosts"
"$BIN/farsight-desktop" --config-dir "$OUT/client" --print-key > "$OUT/server/authorized_keys"
rm -f "$OUT/ctl"; mkfifo "$OUT/ctl"
env -u DISPLAY RUST_LOG=info,smithay=warn,farsight_server::clipboard=debug setsid "$BIN/farsight-server" --port $PORT \
	--no-audio --config-dir "$OUT/server" -- labwc -C "$OUT/cfg" -s \
	"alacritty -o window.dimensions.columns=60 -o window.dimensions.lines=12 -e sh" \
	> "$OUT/server.log" 2>&1 < "$OUT/ctl" &
pids+=($!)
exec 7>"$OUT/ctl"
sleep 2
INNER=$XDG_RUNTIME_DIR/farsight-$PORT/wayland-0
outer() { timeout 10 env WAYLAND_DISPLAY=$OUTER "$WLTOOL" "$@"; }
inner() { timeout 10 env WAYLAND_DISPLAY=$INNER "$WLTOOL" "$@"; }

# A keyboard, so that windows get keyboard focus, which clipboards need.
env WAYLAND_DISPLAY=$OUTER setsid "$WLTOOL" keyboard & pids+=($!)
# Copied before the client window has focus: it offers on focus.
env WAYLAND_DISPLAY=$OUTER setsid "$WLTOOL" copy "copied on the client" & pids+=($!)
env -u DISPLAY WAYLAND_DISPLAY=$OUTER RUST_LOG=info,farsight_desktop=debug setsid "$BIN/farsight-desktop" \
	--no-audio --config-dir "$OUT/client" --size 960x540 127.0.0.1:$PORT > "$OUT/client.log" 2>&1 < /dev/null &
pids+=($!)
sleep 3
outer move 640 360; outer click; sleep 1

echo "1. client → session: $(inner paste || echo FAILED)"

env WAYLAND_DISPLAY=$INNER setsid "$WLTOOL" copy "copied in the session" & pids+=($!)
sleep 1
echo "2. session → client: $(outer paste || echo FAILED)"

echo "text typed from the server" >&7
sleep 0.5
outer commit " and from the input method on the client" || echo "   (the client's input method found no active text field)"
# Enter, through the client window: the shell runs the line.
outer key 28
sleep 1
outer shot "$OUT/text.ppm" > /dev/null
python3 -c "import sys; from PIL import Image; Image.open(sys.argv[1]).save(sys.argv[2])" "$OUT/text.ppm" "$OUT/text.png"
echo "3. text input: $(grep -o 'control: text.*' "$OUT/server.log" | head -1); screenshot $OUT/text.png"
echo quit >&7
sleep 1
