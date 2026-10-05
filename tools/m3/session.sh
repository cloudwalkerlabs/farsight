#!/bin/bash
# M3 session run on one machine, as tools/m1/e2e.sh: what happens to the
# session as clients come and go and the desktop crashes.
#
#   tools/m3/session.sh OUTDIR
#
# 1. client A connects; B joins view-only; both get the picture;
# 2. C connects and takes over: A is closed, B keeps watching;
# 3. the server stops answering for 12 s (SIGSTOP): C and B lose their
#    connections, keep their pictures and reconnect when it's back;
# 4. the desktop crashes: the server restarts it, clients stay connected;
# 5. the desktop exits cleanly: the session ends and the clients are told.
#
# Needs what tools/m1/e2e.sh needs. Logs and screenshots go in OUTDIR.
set -e
OUT=$(mkdir -p "$1" && cd "$1" && pwd)
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=$ROOT/target/release
WLTOOL=$ROOT/tools/m1/wltool/target/release/wltool
PORT=7796
export NO_COLOR=1

mkdir -p "$OUT/cfg"
printf '<?xml version="1.0"?>\n<labwc_config/>\n' > "$OUT/cfg/rc.xml"
pids=()
cleanup() {
	for p in "${pids[@]}"; do kill -CONT -- "-$p" 2>/dev/null || true; kill -- "-$p" 2>/dev/null || true; done
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

mkdir -p "$OUT/server"
for c in a b c; do
	rm -f "$OUT/$c/known_hosts"
	"$BIN/farsight-desktop" --config-dir "$OUT/$c" --print-key >> "$OUT/server/authorized_keys.new"
done
mv "$OUT/server/authorized_keys.new" "$OUT/server/authorized_keys"

env -u DISPLAY RUST_LOG=info,smithay=warn setsid "$BIN/farsight-server" --port $PORT --no-audio \
	--config-dir "$OUT/server" -- labwc -C "$OUT/cfg" -s \
	"alacritty -o window.dimensions.columns=80 -o window.dimensions.lines=20 -e sh -c 'echo the same session; date; sleep 1000'" \
	> "$OUT/server.log" 2>&1 < /dev/null &
SERVER=$!
pids+=($SERVER)
sleep 2
client() {
	local name=$1; shift
	env -u DISPLAY WAYLAND_DISPLAY=$OUTER RUST_LOG=info setsid "$BIN/farsight-desktop" --no-audio \
		--config-dir "$OUT/$name" --size 960x540 "$@" 127.0.0.1:$PORT > "$OUT/$name.log" 2>&1 < /dev/null &
	pids+=($!)
	eval "PID_$name=$!"
}
alive() { kill -0 "$1" 2>/dev/null && echo running || echo exited; }
nested() { pgrep -P "$SERVER" -x labwc | head -1; }
epochs() { grep -c 'new epoch' "$OUT/$1.log" || true; }
shot() {
	WAYLAND_DISPLAY=$OUTER "$WLTOOL" shot "$OUT/$1.ppm" > /dev/null
	python3 -c "import sys; from PIL import Image; Image.open(sys.argv[1]).save(sys.argv[2])" "$OUT/$1.ppm" "$OUT/$1.png"
	rm "$OUT/$1.ppm"
}

echo "1. A controls, B watches"
client a; sleep 2; client b --view-only; sleep 2
echo "   A: $(alive $PID_a), epochs $(epochs a); B: $(alive $PID_b), epochs $(epochs b)"
DESKTOP=$(nested)

echo "2. C takes over"
client c; sleep 2
echo "   A: $(alive $PID_a) ($(grep -o 'another client took over' "$OUT/a.log" | head -1)); B: $(alive $PID_b); C: $(alive $PID_c), epochs $(epochs c)"

echo "3. the server stops answering for 12 s"
kill -STOP "$SERVER"; sleep 12; kill -CONT "$SERVER"; sleep 4
echo "   B: $(alive $PID_b), $(grep -c '^.*reconnected' "$OUT/b.log") reconnect(s); C: $(alive $PID_c), $(grep -c 'reconnected' "$OUT/c.log") reconnect(s)"
echo "   the desktop is the same process: $([ "$(nested)" = "$DESKTOP" ] && echo yes || echo no)"
shot 3-reconnected

echo "4. the desktop crashes"
before_c=$(epochs c)
kill -SEGV "$(nested)"; sleep 4
echo "   restarted: $([ -n "$(nested)" ] && [ "$(nested)" != "$DESKTOP" ] && echo yes || echo no); C: $(alive $PID_c), new epochs $(( $(epochs c) - before_c ))"
shot 4-restarted

echo "5. the desktop exits cleanly"
kill -TERM "$(nested)"; sleep 3
echo "   server: $(alive $SERVER); B: $(alive $PID_b) ($(tail -1 "$OUT/b.log" | cut -c1-60)); C: $(alive $PID_c) ($(tail -1 "$OUT/c.log" | cut -c1-60))"
