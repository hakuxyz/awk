#!/usr/bin/env bash
# Compare this awk against gawk on the same workloads.
# Usage: tools/bench.sh [path-to-awk]
set -euo pipefail

OURS="${1:-$(cd "$(dirname "$0")/.." && pwd)/target/release/awk}"
GAWK="${GAWK:-gawk}"

if [ ! -x "$OURS" ]; then
    echo "build first: cargo build --release" >&2
    exit 1
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
seq 1 3000000 > "$TMP/seq3m"
seq 1 1000000 > "$TMP/seq1m"
printf 'a:b\n' > "$TMP/col.txt"

printf '%-26s %12s %12s\n' "workload" "awk" "gawk"

# bench <description> <input-file> <awk-script> [extra options before script]
bench() {
    local desc="$1" input="$2" script="$3"
    shift 3
    local s e ours gawk
    s=$(date +%s%N)
    "$OURS" "$@" "$script" < "$input" >/dev/null 2>&1
    e=$(date +%s%N)
    ours=$(( (e - s) / 1000000 ))
    s=$(date +%s%N)
    "$GAWK" "$@" "$script" < "$input" >/dev/null 2>&1
    e=$(date +%s%N)
    gawk=$(( (e - s) / 1000000 ))
    printf '%-26s %8d ms %8d ms\n' "$desc" "$ours" "$gawk"
}

bench "sum 3M records" "$TMP/seq3m" '{s+=$1} END{print s}'
bench "print \$1 \$3, 3M records" "$TMP/seq3m" '{print $1, $3}'
bench "regex filter 1M records" "$TMP/seq1m" '/[0-9][0-9]$/{n++} END{print n}'
bench "regex FS, 1M records" "$TMP/seq1m" '{print $2}' -F'[0-9]+'
bench "gsub, 1M records" "$TMP/seq1m" '{gsub(/a/,"b"); print length($0)}'
bench "string FS, 1M records" "$TMP/seq1m" '{print $2}' -F'0'

# startup cost: tiny job repeated
s=$(date +%s%N)
for _ in $(seq 1000); do "$OURS" -F: '{print $1, $3}' /etc/passwd >/dev/null 2>&1; done
e=$(date +%s%N)
ours=$(( (e - s) / 1000000 ))
s=$(date +%s%N)
for _ in $(seq 1000); do "$GAWK" -F: '{print $1, $3}' /etc/passwd >/dev/null 2>&1; done
e=$(date +%s%N)
gawk=$(( (e - s) / 1000000 ))
printf '%-26s %8d ms %8d ms\n' "startup x1000 (/etc/passwd)" "$ours" "$gawk"
