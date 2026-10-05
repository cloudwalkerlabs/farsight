#!/bin/sh
# M0 latency run: farsight-server + labwc + the probe app, 8 seconds.
#
#   tools/m0/latency.sh OUTDIR NAME SIZE [INTERVAL_MS] [FULLSCREEN]
#
# INTERVAL_MS empty: the probe animates on frame callbacks; N: paints every
# N ms. FULLSCREEN 0: windowed, so labwc composites it; 1 (default):
# fullscreen, so labwc passes its buffer straight through.
# Needs: cargo build --release -p farsight-server --bin farsight-server --example probe
set -e
OUT=$1; NAME=$2; SIZE=$3
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$OUT"
(sleep 8; echo quit) | NO_COLOR=1 RUST_LOG=info,smithay=warn \
  PROBE_INTERVAL_MS=$4 PROBE_FULLSCREEN=${5:-1} \
  timeout 30 "$ROOT/target/release/farsight-server" --size "$SIZE" --probe \
  --out "$OUT/$NAME.h264" -- labwc -s "$ROOT/target/release/examples/probe" \
  > "$OUT/$NAME.log" 2>&1
python3 "$ROOT/tools/m0/analyze.py" "$OUT/$NAME.log"
