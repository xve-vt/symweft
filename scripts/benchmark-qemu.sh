#!/usr/bin/env bash
set -euo pipefail
project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_root="${1:-$project_root/test_code/qemu}"
output_root="${2:-$project_root/build/benchmarks/run-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$output_root"
output_root="$(cd "$output_root" && pwd)"
if [[ -e "$output_root/data/pack/snapshots.dir" ]]; then
    echo "Please select a fresh output directory." >&2
    exit 2
fi
cargo build --locked --offline --release --package symweft-core --manifest-path "$project_root/Cargo.toml" --target-dir "$project_root/target"
cargo build --locked --offline --release --package symweft-index --example qemu --manifest-path "$project_root/Cargo.toml" --target-dir "$project_root/target"
for phase in analyze backends hybrid compare replay service export; do
    /usr/bin/time -l "$project_root/target/release/examples/qemu" "$phase" "$source_root" "$output_root/data" \
        > "$output_root/$phase.json" 2> "$output_root/$phase-resource.txt"
done
for phase in full-index reindex-unchanged; do
    SYMWEFT_CACHE_DIR="$output_root/runtime-cache" /usr/bin/time -l \
        "$project_root/target/release/symweft-core" index "$source_root" \
        > "$output_root/$phase.json" 2> "$output_root/$phase-resource.txt"
done
echo "Benchmark results: $output_root"
echo "This script uses macOS time -l. OS page cache is not purged; analysis is A1 syntax only."
