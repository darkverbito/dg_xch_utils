#!/bin/bash
# pi_harvest.sh — collect everything the two's-complement-limb decision needs from a
# live Pi running the dg_xch full node (mmap profile). Run ON the Pi (or any host that
# can reach the node's metrics port). Produces a single tarball to attach/scp back.
#
#   ./pi_harvest.sh                       # defaults: node on localhost:9100, 30 min
#   NODE=10.0.0.42:9100 MINUTES=60 ./pi_harvest.sh
#
# What it collects and why:
#   flamegraph-N.svg      CPU attribution: % of wall in class-group mul/sqr/reduce —
#                         the ceiling on what the limb rework can buy. 3 samples.
#   metrics.csv           per-minute: peak height, blocks confirmed, window.vdf/body/
#                         confirm wall µs, RSS, jemalloc allocated — VDF throughput
#                         and memory headroom at Pi scale.
#   thermal.csv           per-minute: ARM clock, SoC temp, get_throttled register —
#                         a throttled run is not a valid measurement; this proves
#                         whether the numbers are clean.
#   hardware.txt          model/revision/RAM/kernel/storage — comparability record.
#   node_tail.log         last 2000 node log lines (window spans, walls, restarts).
set -u
NODE="${NODE:-localhost:9100}"
MINUTES="${MINUTES:-30}"
OUT="pi_harvest_$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$OUT"

echo "== hardware identity" | tee "$OUT/hardware.txt"
{
  cat /proc/device-tree/model 2>/dev/null; echo
  grep -E "Revision|Model" /proc/cpuinfo
  uname -a
  free -h
  lsblk -o NAME,SIZE,TYPE,MOUNTPOINT,MODEL 2>/dev/null
  findmnt -no SOURCE,FSTYPE / 2>/dev/null
} >> "$OUT/hardware.txt" 2>&1

# Node identity: which build is running.
curl -s --max-time 10 "http://$NODE/metrics" | head -5 > "$OUT/node_probe.txt" 2>&1 || \
  echo "WARN: metrics endpoint unreachable at $NODE" | tee -a "$OUT/node_probe.txt"

echo "ts,peak_height,blocks_confirmed_total,window_vdf_us,window_body_us,window_confirm_us,rss_bytes,alloc_allocated_bytes" > "$OUT/metrics.csv"
echo "ts,arm_clock_hz,temp_c,throttled_hex" > "$OUT/thermal.csv"

metric() { echo "$1" | grep -E "^fullnode_$2 " | awk '{print $2}' | head -1; }

FLAME_AT=(2 $((MINUTES/2)) $((MINUTES-2)))  # early / mid / late CPU samples
for ((i=0; i<MINUTES; i++)); do
  TS=$(date -u +%H:%M:%S)
  M=$(curl -s --max-time 10 "http://$NODE/metrics" 2>/dev/null)
  echo "$TS,$(metric "$M" peak_height),$(metric "$M" blocks_confirmed_total),$(metric "$M" window_vdf_micros),$(metric "$M" window_body_micros),$(metric "$M" window_confirm_micros),$(metric "$M" process_resident_bytes),$(metric "$M" alloc_allocated_bytes)" >> "$OUT/metrics.csv"
  if command -v vcgencmd >/dev/null 2>&1; then
    echo "$TS,$(vcgencmd measure_clock arm | cut -d= -f2),$(vcgencmd measure_temp | grep -oE '[0-9.]+'),$(vcgencmd get_throttled | cut -d= -f2)" >> "$OUT/thermal.csv"
  else
    T=$(cat /sys/class/thermal/thermal_zone0/temp 2>/dev/null)
    F=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq 2>/dev/null)
    echo "$TS,${F:-na}000,$(awk "BEGIN{print ${T:-0}/1000}"),na" >> "$OUT/thermal.csv"
  fi
  for f in "${FLAME_AT[@]}"; do
    if [ "$i" -eq "$f" ]; then
      echo "  [$TS] sampling flamegraph ($((f))m mark, ~20s)..."
      curl -s --max-time 60 "http://$NODE/debug/flamegraph" -o "$OUT/flamegraph-${f}m.svg" || true
    fi
  done
  sleep 60
done

# Node log tail: journald service, docker container, or k8s-style — take whichever exists.
{ journalctl -u dg-xch-node -n 2000 --no-pager 2>/dev/null || \
  docker logs --tail 2000 "$(docker ps --format '{{.Names}}' 2>/dev/null | grep -m1 -i 'dg.xch\|full.node')" 2>&1 || \
  echo "no journald unit or docker container found — capture the node log manually"; } > "$OUT/node_tail.log"

tar czf "$OUT.tar.gz" "$OUT"
echo
echo "DONE → $OUT.tar.gz  (scp this back)"
echo "Quick self-check:"
awk -F, 'NR>1 && $4!="" {n++; s+=$4} END {if (n) printf "  window.vdf mean: %.1f ms over %d samples\n", s/n/1000, n}' "$OUT/metrics.csv"
awk -F, 'NR>1 && $4!="0x0" && $4!="na" && $4!="" {t++} END {printf "  throttled samples: %d (0 = clean run)\n", t+0}' "$OUT/thermal.csv"
