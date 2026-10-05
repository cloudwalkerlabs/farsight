"""Join the probe's commit times with the server's per-frame log.

The probe and the server log to the same file (labwc passes stdout on).
"""
import re
import statistics as st
import sys

log = sys.argv[1]
commits, rows = {}, []
for line in open(log):
    m = re.match(r"probe t=(\d+) commit seq=(\d+)", line)
    if m:
        commits[int(m[2])] = int(m[1])
    elif " frame: frame " in line:
        rows.append(dict(re.findall(r"(\w+)=(\S+)", line.split(" frame: frame ")[1])))

stages = {
    "app → host commit": [], "import": [], "probe readback": [],
    "convert": [], "encode": [], "host total": [], "app → encoded": [],
}
matched = 0
for f in rows[5:]:  # skip start-up and the first epoch change
    us = lambda k: int(f[k])
    stages["import"].append(us("import_us"))
    stages["probe readback"].append(us("probe_readback_us"))
    stages["convert"].append(us("convert_us"))
    stages["encode"].append(us("encode_us"))
    stages["host total"].append(us("total_us"))
    seq = int(f["probe"])
    if seq in commits:
        matched += 1
        a = us("t_commit_mono_us") - commits[seq]
        stages["app → host commit"].append(a)
        stages["app → encoded"].append(a + us("total_us"))

def pct(v, p):
    v = sorted(v)
    return v[min(len(v) - 1, int(p * len(v)))]

print(f"{log}: {len(rows)} frames, {matched} matched to {len(commits)} probe commits")
for name, v in stages.items():
    if v:
        print(f"  {name:18s} median {st.median(v)/1000:6.2f} ms  p95 {pct(v, .95)/1000:6.2f} ms  max {max(v)/1000:6.2f} ms")
