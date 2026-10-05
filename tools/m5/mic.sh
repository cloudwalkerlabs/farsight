#!/bin/bash
# M5 microphone run on one machine, as tools/m3/audio.sh: a headless labwc
# stands in for the client's desktop, and the client has a private PipeWire
# (tools/audio/isolated-session.sh) whose microphone is a virtual source.
# What the client plays comes back into that source 20 ms later, as an
# echo from its speakers would (their latency, and the room); a near-end
# voice (a 700 Hz tone) is played into it too, later.
#
# In the remote session, an app records from farsight-mic while another
# plays pink noise (the far end, which must not come back). Afterwards the
# recording is cut into the far end's stretch and the near end's: what is
# left of the noise (echo) and how loud the tone is (near).
#
#   tools/m5/mic.sh OUTDIR                          # echo cancelled
#   CLIENT_ARGS=--no-echo-cancel tools/m5/mic.sh OUTDIR
#
# Needs what tools/m3/audio.sh needs, plus pactl and pw-loopback.
set -e
OUT=$(mkdir -p "$1" && cd "$1" && pwd)
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=$ROOT/target/release
PORT=7798
export NO_COLOR=1

mkdir -p "$OUT/cfg"
printf '<?xml version="1.0"?>\n<labwc_config/>\n' > "$OUT/cfg/rc.xml"
# Far end: 8 s of pink noise. Near end: 4 s of a 700 Hz tone, after it.
ffmpeg -loglevel error -y -f lavfi -i "anoisesrc=color=pink:amplitude=0.3:sample_rate=48000:duration=8" \
	-ac 2 "$OUT/far.wav"
ffmpeg -loglevel error -y -f lavfi -i "sine=frequency=700:sample_rate=48000:duration=4" \
	-af "volume=0.3" -ac 1 "$OUT/near.wav"

pids=()
cleanup() {
	exec 7>&- 2>/dev/null || true
	for p in "${pids[@]}"; do kill -- "-$p" 2>/dev/null || true; done
	[ -n "$CA" ] && kill $(cat "$CA"/*.pid 2>/dev/null) 2>/dev/null || true
}
trap cleanup EXIT

# The client's sound server: private, with a null sink for its speaker and
# a virtual source for its microphone that hears the speaker.
CA=/tmp/fsm-$(id -u)
"$ROOT/tools/audio/isolated-session.sh" "$CA" > /dev/null
export_ca() { XDG_RUNTIME_DIR=$CA PULSE_SERVER=unix:$CA/pulse/native "$@"; }
export_ca pactl load-module module-null-sink sink_name=fakemic media.class=Audio/Source/Virtual \
	channel_map=mono > /dev/null
export_ca pactl set-default-source fakemic
sleep 0.5

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

mkdir -p "$OUT/server"; rm -f "$OUT/client/known_hosts" "$OUT/rec.wav"
"$BIN/farsight-desktop" --config-dir "$OUT/client" --print-key > "$OUT/server/authorized_keys"
rm -f "$OUT/ctl"; mkfifo "$OUT/ctl"
# In the session: record from 4 s to 22 s, and play the far end from 6 s.
SESSION="sleep 4; pw-record --target farsight-mic --channels 1 $OUT/rec.wav & R=\$!; sleep 2; pw-play $OUT/far.wav; sleep 10; kill \$R"
env -u DISPLAY RUST_LOG=info,smithay=warn setsid "$BIN/farsight-server" --port $PORT $SERVER_ARGS \
	--config-dir "$OUT/server" -- labwc -C "$OUT/cfg" -s "sh -c '$SESSION'" \
	> "$OUT/server.log" 2>&1 < "$OUT/ctl" &
pids+=($!)
exec 7>"$OUT/ctl"
sleep 2
env -u DISPLAY XDG_RUNTIME_DIR=$CA WAYLAND_DISPLAY=$XDG_RUNTIME_DIR/$OUTER RUST_LOG=info \
	setsid "$BIN/farsight-desktop" --config-dir "$OUT/client" 127.0.0.1:$PORT --size 1280x720 --mic $CLIENT_ARGS \
	> "$OUT/client.log" 2>&1 < /dev/null &
pids+=($!)
# The echo: what the speaker plays, into the microphone, 20 ms later.
# WirePlumber links the loopback's output to the speaker; it goes to the
# microphone instead.
export_ca setsid pw-loopback -n echo -C auto_null -d 0.02 -i '{ stream.capture.sink = true }' \
	> "$OUT/loopback.log" 2>&1 < /dev/null &
pids+=($!)
sleep 1
for c in FL FR; do
	export_ca pw-link -d output.echo:output_$c auto_null:playback_$c 2>/dev/null || true
	export_ca pw-link output.echo:output_$c fakemic:input_MONO
done
# The near end, at about 16 s, after the far end.
sleep 14
export_ca pw-play --target fakemic "$OUT/near.wav"
sleep 6
echo quit >&7
sleep 1
grep -o 'microphone.*' "$OUT/server.log" "$OUT/client.log" | sed 's/^/  /' || true

# RMS per half second: everything but the tone (what is left of the far
# end), and the tone alone (the near end).
levels() {
	ffmpeg -nostats -loglevel info -i "$OUT/rec.wav" -af "highpass=f=100,$1,asetnsamples=24000,astats=metadata=1:reset=1,ametadata=print:key=lavfi.astats.Overall.RMS_level" \
		-f null - 2>&1 | grep -o 'RMS_level=[-0-9.inf]*' | cut -d= -f2
}
levels "bandreject=f=700:width_type=h:w=200" > "$OUT/echo.txt"
levels "bandpass=f=700:width_type=h:w=100" > "$OUT/near.txt"
python3 - "$OUT" <<'PY'
import statistics, sys
out = sys.argv[1]
read = lambda name: [float(v) if v not in ("-inf", "inf") else -150.0 for v in open(f"{out}/{name}").read().split()]
echo, near = read("echo.txt"), read("near.txt")
# Half-second windows from 4 s into the session: the far end plays from
# window 4 for 8 s; the near end is wherever the tone stands out.
far = echo[4:20]
tone = [n for i, (n, e) in enumerate(zip(near, echo)) if i >= 20 and n > -50]
fmt = lambda v: f"{v:.0f}"
print(f"far end left (echo), dBFS: first second {fmt(max(far[:2]))}, then median {fmt(statistics.median(far[2:]))}"
      f" (max {fmt(max(far[2:]))}); near end {fmt(statistics.median(tone)) if tone else 'missing'} dBFS")
PY
