#!/bin/bash
# M4: one run over an impaired network, on one machine, as root of nothing.
# The script runs itself again inside a new user and network namespace
# (`unshare -rn`), where it may put `tc netem` on that namespace's
# loopback; the server, the client, the client's headless desktop and its
# private sound server all run in there. Loopback carries both directions,
# so the impairment applies each way: `delay 10ms` is a 20 ms round trip.
# Give jitter a rate, as above: without one, netem reorders packets freely,
# which real links seldom do.
#
#   tools/m4/netem.sh OUTDIR "NETEM" [SECONDS]
#   tools/m4/netem.sh /tmp/m4 "loss 5%"
#   tools/m4/netem.sh /tmp/m4 "delay 10ms 2ms rate 1gbit loss 5%" 30
#   tools/m4/netem.sh /tmp/m4 ""                  # no impairment
#
# The session plays a 440 Hz tone (from 4 s in) under es2gears, which keeps
# video moving. Prints the client's video and audio statistics; the client
# played into OUTDIR/played.wav.
#
# Needs what tools/m3/audio.sh needs, plus iproute2's tc and the sch_netem
# module. SERVER_ARGS, CLIENT_ARGS and SESSION_APP as for tools/m1/e2e.sh;
# SERVER_LOG and CLIENT_LOG are their RUST_LOG.
#
# CHECK_FRAMES=1 checks that every picture the client decoded is exactly
# what decoding the server's whole stream gives: the client decodes in
# software and hashes each picture, and FFmpeg decodes the stream the
# server wrote (--out). A frame decoded against a missing or wrong
# reference doesn't match.
set -e
if [ -z "$FARSIGHT_NETNS" ]; then
	exec unshare -rn env FARSIGHT_NETNS=1 "$0" "$@"
fi
# netem drops a UDP segmentation offload batch whole, where a real link
# loses its packets one by one: send them one by one.
export FARSIGHT_NO_GSO=1
OUT=$(mkdir -p "$1" && cd "$1" && pwd)
NETEM=$2
SECS=${3:-20}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=$ROOT/target/release
PORT=7796
export NO_COLOR=1

ip link set lo up
[ -n "$NETEM" ] && tc qdisc add dev lo root netem $NETEM
tc qdisc show dev lo > "$OUT/netem.txt"

mkdir -p "$OUT/cfg"
printf '<?xml version="1.0"?>\n<labwc_config/>\n' > "$OUT/cfg/rc.xml"
ffmpeg -loglevel error -y -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=$SECS" \
	-af "volume=0.5" -ac 2 "$OUT/tone.wav"

pids=()
cleanup() {
	exec 7>&- 2>/dev/null || true
	for p in "${pids[@]}"; do kill -- "-$p" 2>/dev/null || true; done
	[ -n "$CA" ] && kill $(cat "$CA"/*.pid 2>/dev/null) 2>/dev/null || true
}
trap cleanup EXIT

# The client's sound server: private, with only a null sink.
CA=/tmp/fsn-$(id -u)-$$
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

APP=${SESSION_APP:-es2gears_wayland}
if [ -n "$CHECK_FRAMES" ]; then
	SERVER_ARGS="$SERVER_ARGS --out $OUT/stream.bin"
	CLIENT_ARGS="$CLIENT_ARGS --software"
	export FARSIGHT_FRAME_MD5=$OUT/client.md5
fi
mkdir -p "$OUT/server"; rm -f "$OUT/client/known_hosts"
"$BIN/farsight-desktop" --config-dir "$OUT/client" --print-key > "$OUT/server/authorized_keys"
rm -f "$OUT/ctl"; mkfifo "$OUT/ctl"
env -u DISPLAY RUST_LOG=${SERVER_LOG:-info,smithay=warn} setsid "$BIN/farsight-server" --port $PORT $SERVER_ARGS \
	--config-dir "$OUT/server" -- labwc -C "$OUT/cfg" -s "sh -c '(sleep 4; pw-play $OUT/tone.wav) & exec $APP'" \
	> "$OUT/server.log" 2>&1 < "$OUT/ctl" &
pids+=($!)
exec 7>"$OUT/ctl"
sleep 2
env -u DISPLAY RUST_LOG=${CLIENT_LOG:-info} XDG_RUNTIME_DIR=$CA WAYLAND_DISPLAY=$XDG_RUNTIME_DIR/$OUTER \
	setsid "$BIN/farsight-desktop" --config-dir "$OUT/client" 127.0.0.1:$PORT --size 1280x720 $CLIENT_ARGS \
	> "$OUT/client.log" 2>&1 < /dev/null &
pids+=($!)
XDG_RUNTIME_DIR=$CA timeout $((SECS + 4)) pw-record -P '{ stream.capture.sink = true }' --target auto_null \
	"$OUT/played.wav" 2>/dev/null &
REC=$!
wait $REC || true
echo quit >&7
sleep 1
echo "netem: $(cat "$OUT/netem.txt")"
grep -o 'fps=.*' "$OUT/client.log" | sed 's/^/  video: /' || true
grep -o 'audio: .*' "$OUT/client.log" | sed 's/^/  /' || true
if [ -n "$CHECK_FRAMES" ]; then
	codec=$(grep -o 'encoding=[a-z0-9]*' "$OUT/server.log" | tail -1 | cut -d= -f2)
	ffmpeg -nostdin -y -loglevel fatal -f "$codec" -i "$OUT/stream.bin" -fps_mode passthrough -f framemd5 "$OUT/stream.md5"
	python3 - "$OUT/stream.md5" "$OUT/client.md5" <<'PY'
import sys
from collections import Counter
ref = [l.split(",")[-1].strip() for l in open(sys.argv[1]) if not l.startswith("#")]
got = [(int(n), md5) for n, md5 in (l.split() for l in open(sys.argv[2]))]
# The server's file holds the last epoch, which starts at some frame:
# line up on the offset most frames agree on, and leave out frames of
# earlier epochs.
where = {}
for i, md5 in enumerate(ref):
    where.setdefault(md5, []).append(i)
offsets = Counter(i - n for n, md5 in got for i in where.get(md5, []))
off = offsets.most_common(1)[0][0] if offsets else 0
got = [(n, md5) for n, md5 in got if n + off >= 0]
bad = [n for n, md5 in got if n + off >= len(ref) or ref[n + off] != md5]
print(f"  frames: {len(got) - len(bad)} of {len(got)} decoded exactly as the whole stream decodes"
      + (f"; differ: {bad[:10]}" if bad else ""))
PY
fi
