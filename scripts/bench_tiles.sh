#!/usr/bin/env bash
# Tile benchmark: cold + warm pass, parse stage timings from perf-tracing logs.
# Usage: ./scripts/bench_tiles.sh [png|webp|jpeg] [cache_mb]
set -uo pipefail

FORMAT=${1:-png}
CACHE_MB=${2:-512}
PORT=19876
DATASET="SVN3-01_20250814_L2D0000203064_01_6112500050210001-FUS_3857_cog.tif"
BINARY="target/profiling/examples/serve"
LOG_FILE="/tmp/async-geotiff-bench-$FORMAT.log"
SUMMARY="/tmp/async-geotiff-bench-$FORMAT-summary.txt"

# z14 tile matrix matching the benchmark doc: x 13652..13667 (16), y 6182..6198 (17) = 272 tiles
TILES=()
for x in $(seq 13652 13667); do
  for y in $(seq 6182 6198); do
    TILES+=("$x/$y")
  done
done
echo "Tile count: ${#TILES[@]}"

# Generate URL list
urls() {
  for xy in "${TILES[@]}"; do
    echo "http://127.0.0.1:$PORT/tiles/WebMercatorQuad/14/$xy.$FORMAT?dataset=$DATASET"
  done
}

run_pass() {
  local label=$1
  echo "=== $label pass ===" | tee -a "$LOG_FILE"
  urls | xargs -P 64 -I{} curl -sf -o /dev/null -w "%{time_total}\n" {} | \
    sort -n | awk -v label="$label" '{vals[NR]=$1; n=NR}
         END {
           p50=vals[int(n*0.50)+1];
           p95=vals[int(n*0.95)+1];
           p99=vals[int(n*0.99)+1];
           printf "%s: n=%d p50=%.1fms p95=%.1fms p99=%.1fms\n",
             label, n, p50*1000, p95*1000, p99*1000
         }' | tee -a "$SUMMARY"
}

# Start server
echo "Starting server (cache=${CACHE_MB}MB, format=$FORMAT)..."
ASYNC_GEOTIFF_BLOCK_CACHE_MB=$CACHE_MB \
ASYNC_GEOTIFF_MAX_DECODE_TASKS=6 \
ASYNC_GEOTIFF_MAX_TILE_TASKS=4 \
ASYNC_GEOTIFF_BIND=127.0.0.1:$PORT \
RUST_LOG=async_geotiff=debug \
  "$BINARY" > "$LOG_FILE" 2>&1 &
SERVER_PID=$!
trap "kill $SERVER_PID 2>/dev/null; echo 'server stopped'" EXIT

# Wait for server ready
echo -n "Waiting for server..."
for i in $(seq 1 30); do
  if curl -sf "http://127.0.0.1:$PORT/formats" >/dev/null 2>&1; then
    echo " ready (${i}s)"
    break
  fi
  sleep 1
  echo -n "."
done

echo "" > "$SUMMARY"
echo "FORMAT=$FORMAT CACHE_MB=$CACHE_MB MAX_DECODE=6 MAX_TILE=4" >> "$SUMMARY"
echo "Tile matrix: z14, x13652-13667, y6182-6198 (${#TILES[@]} tiles), concurrency=8" >> "$SUMMARY"
echo "" >> "$SUMMARY"

run_pass "cold"
echo "Sleeping 2s before warm pass..."
sleep 2
run_pass "warm"

echo ""
echo "=== Stage timing summary (from debug logs) ==="

# Parse perf-tracing stage lines from server log
# Format: ... stages="blocks_ms=X.Xms data_stitch_ms=X.Xms render_encode_ms=X.Xms ..." total_ms=X.X ...
python3 - "$LOG_FILE" "$SUMMARY" <<'EOF'
import sys, re, statistics

log_path = sys.argv[1]
summary_path = sys.argv[2]

fields = ["blocks_ms", "data_stitch_ms", "data_stitch_job_ms",
          "render_encode_ms", "total_ms", "warp_cpu_ms"]

data = {f: [] for f in fields}

with open(log_path) as f:
    for line in f:
        if "tile timing" not in line:
            continue
        for field in fields:
            m = re.search(rf'{field}=([0-9.]+)', line)
            if m:
                data[field].append(float(m.group(1)))

def pct(vals, p):
    if not vals:
        return float('nan')
    s = sorted(vals)
    idx = int(len(s) * p / 100)
    return s[min(idx, len(s)-1)]

lines = ["\n=== Stage timings from server debug log ===",
         f"{'Stage':<28} {'n':>5} {'P50':>8} {'P95':>8} {'P99':>8}"]

for field in fields:
    vals = data[field]
    if not vals:
        continue
    row = f"{field:<28} {len(vals):>5} {pct(vals,50):>7.3f}ms {pct(vals,95):>7.3f}ms {pct(vals,99):>7.3f}ms"
    lines.append(row)

output = "\n".join(lines)
print(output)
with open(summary_path, "a") as f:
    f.write(output + "\n")

print(f"\nFull log: {log_path}")
print(f"Summary:  {summary_path}")
EOF
