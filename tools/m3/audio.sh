#!/bin/bash
# M3 audio run on one machine, as tools/m1/e2e.sh: a headless labwc stands in
# for the client's desktop, and the client plays into a private PipeWire of
# its own (tools/audio/isolated-session.sh), so nothing reaches this
# machine's speakers. In the remote session, an app plays a tone through
# the session's default sink, farsight-speaker.
#
#   tools/m3/audio.sh OUTDIR
#
# Writes OUTDIR/played.wav (what the client played, from its null sink's
# monitor), and prints the client's audio statistics, what the session's
# PipeWire has for devices, and whether this machine's own PipeWire saw any
# of it.
#
# Needs what tools/m1/e2e.sh needs, plus ffmpeg, pw-cat and pw-cli.
# SERVER_ARGS and CLIENT_ARGS are passed on; SECONDS_PLAYED (default 20).
set -e
OUT=$(mkdir -p "$1" && cd "$1" && pwd)
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=$ROOT/target/release
PORT=7797
PLAY=${SECONDS_PLAYED:-20}
export NO_COLOR=1

mkdir -p "$OUT/cfg"
printf '<?xml version="1.0"?>\n<labwc_config/>\n' > "$OUT/cfg/rc.xml"
# A tone that changes pitch every second, so gaps and repeats are audible.
ffmpeg -loglevel error -y -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=$PLAY" \
	-af "volume=0.5" -ac 2 "$OUT/tone.wav"

pids=()
cleanup() {
	exec 7>&- 2>/dev/null || true
	for p in "${pids[@]}"; do kill -- "-$p" 2>/dev/null || true; done
	[ -n "$CA" ] && kill $(cat "$CA"/*.pid 2>/dev/null) 2>/dev/null || true
}
trap cleanup EXIT

# The client's sound server: private, with only a null sink.
CA=/tmp/fsc-$(id -u)
"$ROOT/tools/audio/isolated-session.sh" "$CA" > /dev/null

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

user_nodes() { pw-cli ls Node 2>/dev/null | grep -c 'node.name' || true; }
NODES_BEFORE=$(user_nodes)

rm -f "$OUT/ctl"; mkfifo "$OUT/ctl"
env -u DISPLAY RUST_LOG=info,smithay=warn setsid "$BIN/farsight-server" --port $PORT $SERVER_ARGS \
	--identity "$OUT/identity" -- labwc -C "$OUT/cfg" -s "sh -c 'sleep 4; pw-play $OUT/tone.wav'" \
	> "$OUT/server.log" 2>&1 < "$OUT/ctl" &
pids+=($!)
exec 7>"$OUT/ctl"
sleep 2
env -u DISPLAY XDG_RUNTIME_DIR=$CA WAYLAND_DISPLAY=$XDG_RUNTIME_DIR/$OUTER \
	setsid "$BIN/farsight-desktop" 127.0.0.1:$PORT --size 1280x720 $CLIENT_ARGS \
	> "$OUT/client.log" 2>&1 < /dev/null &
pids+=($!)
XDG_RUNTIME_DIR=$CA timeout $((PLAY + 4)) pw-record -P '{ stream.capture.sink = true }' --target auto_null \
	"$OUT/played.wav" 2>/dev/null &
REC=$!
sleep 1
SR=$XDG_RUNTIME_DIR/farsight-$PORT
echo "session devices: $(XDG_RUNTIME_DIR=$SR pw-cli ls Device | grep -c device.api || true)"
echo "session sinks:"; XDG_RUNTIME_DIR=$SR pw-cli ls Node | grep -A1 'node.name' | grep -E 'node.name' | sed 's/^\s*/  /'
NODES_DURING=$(user_nodes)
wait $REC || true
echo "this machine's PipeWire nodes: $NODES_BEFORE before, $NODES_DURING while playing"
echo quit >&7
sleep 1
grep -o 'audio: .*' "$OUT/client.log" | sed 's/^/  /' || true
