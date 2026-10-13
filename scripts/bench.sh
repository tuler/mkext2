#!/usr/bin/env bash
# Compare mkext2 and xgenext2fs speed on a generated dataset of about SIZE_MB
# megabytes, checking that both images are valid and hold the same content.
#
#   scripts/bench.sh [SIZE_MB] [WORKDIR]
set -euo pipefail

SIZE_MB=${1:-1024}
WORK=${2:-bench-work}
REF=${XGENEXT2FS:-xgenext2fs}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$WORK"
WORK=$(cd "$WORK" && pwd)

cargo build --release --quiet --manifest-path "$ROOT/Cargo.toml"
cargo build --release --quiet --manifest-path "$ROOT/Cargo.toml" --example gendata --example compare
OURS="$ROOT/target/release/mkext2"

DATA="$WORK/data-$SIZE_MB"
if [ ! -d "$DATA" ]; then
  "$ROOT/target/release/examples/gendata" "$DATA" "$SIZE_MB"
fi
if [ ! -f "$DATA.tar" ]; then
  COPYFILE_DISABLE=1 tar -cf "$DATA.tar" -C "$DATA" .
fi
if [ ! -f "$DATA.tar.gz" ]; then
  gzip -1 -c "$DATA.tar" > "$DATA.tar.gz"
fi

now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }

measure() { # name tool args...
  local name=$1 tool=$2; shift 2
  local best=""
  for _ in 1 2 3; do
    rm -f "$WORK/$name.img"
    sync
    local t0 t1
    t0=$(now)
    "$tool" "$@" "$WORK/$name.img" 2>/dev/null
    t1=$(now)
    local dt
    dt=$(echo "$t1 - $t0" | bc)
    if [ -z "$best" ] || (( $(echo "$dt < $best" | bc) )); then best=$dt; fi
  done
  echo "$best"
}

printf "%-28s %12s %12s %8s %s\n" "case" "xgenext2fs" "mkext2" "speedup" "same content"
run_case() { # label args...
  local label=$1; shift
  local tr to
  tr=$(measure ref "$REF" "$@")
  to=$(measure new "$OURS" "$@")
  local same=no
  "$ROOT/target/release/examples/compare" "$WORK/ref.img" "$WORK/new.img" >/dev/null && same=yes
  printf "%-28s %11.2fs %11.2fs %7.1fx %s\n" "$label" "$tr" "$to" "$(echo "$tr / $to" | bc -l)" "$same"
}

export SOURCE_DATE_EPOCH=1700000000
run_case "directory (4 KiB blocks)" -B 4096 -f -r +1000 -d "$DATA"
run_case "directory (1 KiB blocks)" -B 1024 -f -r +1000 -d "$DATA"
run_case "tar (4 KiB blocks)" -B 4096 -f -r +1000 -a "$DATA.tar"
run_case "tar.gz (4 KiB blocks)" -B 4096 -f -r +1000 -a "$DATA.tar.gz"
run_case "empty 1 GiB image" -B 4096 -f -b 262144
