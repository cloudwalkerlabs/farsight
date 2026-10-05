#!/bin/bash
# M4's test matrix: tools/m4/netem.sh under each network condition below,
# then a table of what came through.
#
#   tools/m4/matrix.sh OUTDIR [NAME...]        # every condition, or these
#
# Each condition runs twice: es2gears and a tone for SECONDS (default 20),
# with CHECK_FRAMES=1 (every picture decoded must match a decode of the
# server's whole stream); and typing TYPE_LINES (default 20) lines into a
# terminal in the session. ENCODERS (default nvenc, which has reference
# frame invalidation) is passed to the server.
#
# The table: video frames lost after FEC and NACK (and RFIs asked), frames
# rebuilt by FEC and repaired by NACK, pictures not bit-exact, the median
# glass-to-glass latency and frame rate; audio frames concealed (and how
# many of those came late rather than not at all) and the median latency;
# lines typed wrong. The first five seconds of each run are left out of
# the video and audio figures: startup, before loss is measured.
set -e
OUT=$(mkdir -p "$1" && cd "$1" && pwd)
shift
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
SECS=${SECONDS_RUN:-20}
LINES=${TYPE_LINES:-20}

declare -A NETEM=(
	[clean]=""
	[loss1]="loss 1%"
	[loss5]="loss 5%"
	[loss10]="loss 10%"
	[loss20]="loss 20%"
	[bursty]="loss gemodel 1% 30% 100% 0%"
	[wan]="delay 10ms 2ms rate 1gbit"
	[wan-loss5]="delay 10ms 2ms rate 1gbit loss 5%"
	[wan-bursty]="delay 10ms 2ms rate 1gbit loss gemodel 1% 30% 100% 0%"
	[far-loss5]="delay 40ms 5ms rate 1gbit loss 5%"
	[8mbit]="rate 8mbit"
	[1mbit]="rate 1mbit"
)
ORDER=(clean loss1 loss5 loss10 loss20 bursty wan wan-loss5 wan-bursty far-loss5 8mbit 1mbit)
[ $# -gt 0 ] && ORDER=("$@")

for name in "${ORDER[@]}"; do
	netem=${NETEM[$name]}
	echo "== $name: ${netem:-no impairment}"
	mkdir -p "$OUT/$name"
	env -u TYPE_LINES CHECK_FRAMES=1 SERVER_ARGS="--encoders ${ENCODERS:-nvenc}" \
		"$ROOT/tools/m4/netem.sh" "$OUT/$name/av" "$netem" "$SECS" > "$OUT/$name/av.txt" 2>&1 || true
	env -u CHECK_FRAMES TYPE_LINES=$LINES SERVER_ARGS="--encoders ${ENCODERS:-nvenc}" \
		"$ROOT/tools/m4/netem.sh" "$OUT/$name/type" "$netem" $((LINES * 2 + 10)) > "$OUT/$name/type.txt" 2>&1 || true
done

python3 - "$OUT" "${ORDER[@]}" <<'PY'
import re, sys, statistics as st
out, names = sys.argv[1], sys.argv[2:]

def kv(line):
    return dict(re.findall(r'(\w+)="?([-\d.]+)"?', line))

rows = []
for name in names:
    log = open(f"{out}/{name}/av/client.log").read().splitlines()
    t0 = None
    video, audio = [], []
    for l in log:
        ts = re.match(r'\S+T(\d+):(\d+):([\d.]+)Z', l)
        if not ts:
            continue
        t = int(ts[1]) * 3600 + int(ts[2]) * 60 + float(ts[3])
        t0 = t if t0 is None else t0
        if t - t0 < 7:
            continue
        if "latency ms" in l:
            m = re.search(r'total ([\d.]+)/', l)
            video.append((kv(l), float(m[1]) if m else None))
        elif "capture→speaker" in l:
            m = re.search(r'p50 ([\d.]+).*concealed (\d+) \(late (\d+)\)', l)
            audio.append((float(m[1]), int(m[2]), int(m[3])))
    s = lambda k: sum(int(float(v.get(k, 0))) for v, _ in video)
    last = video[-1][0] if video else {}
    av = open(f"{out}/{name}/av.txt").read()
    m = re.search(r'frames: (\d+) of (\d+)', av)
    exact = f"{int(m[2]) - int(m[1])}/{m[2]}" if m else "-"
    ty = open(f"{out}/{name}/type.txt").read()
    m = re.search(r'typing: (\d+) of (\d+)', ty)
    typed = f"{int(m[2]) - int(m[1])}/{m[2]}" if m else "-"
    lat = [x for _, x in video if x is not None]
    rows.append([
        name,
        f"{s('lost')} ({s('rfi')})",
        str(s('recovered')),
        str(s('repaired')),
        exact,
        f"{st.median(lat):.1f}" if lat else "-",
        f"{st.mean(float(v['fps']) for v, _ in video):.0f}" if video else "-",
        f"{sum(a[1] for a in audio)} ({sum(a[2] for a in audio)})" if audio else "-",
        f"{st.median(a[0] for a in audio):.1f}" if audio else "-",
        typed,
    ])
head = ["condition", "lost (RFI)", "FEC", "NACK", "not exact", "latency ms", "fps",
        "audio concealed (late)", "audio ms", "typed wrong"]
table = [head, ["---"] * len(head)] + rows
text = "\n".join("| " + " | ".join(r) + " |" for r in table)
open(f"{out}/matrix.md", "w").write(text + "\n")
print(text)
PY
