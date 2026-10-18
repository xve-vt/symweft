#!/usr/bin/env bash
set -euo pipefail
project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo build --locked --offline --package symweft-core --manifest-path "$project_root/Cargo.toml" --target-dir "$project_root/target"
mkdir -p "$project_root/build/checks" "$project_root/build/module-cache"
swiftc -parse-as-library -module-name SymweftChecks \
    -module-cache-path "$project_root/build/module-cache" \
    "$project_root/apps/macos/Sources/Symweft/Models.swift" \
    "$project_root/apps/macos/Sources/Symweft/SemanticModels.swift" \
    "$project_root/apps/macos/Sources/Symweft/Appearance.swift" \
    "$project_root/apps/macos/Sources/Symweft/CoreClient.swift" \
    "$project_root/apps/macos/Sources/Symweft/SourceCoordinates.swift" \
    "$project_root/scripts/check-macos.swift" \
    -o "$project_root/build/checks/mac-foundation-checks"
SYMWEFT_CACHE_DIR="$project_root/build/checks/index-cache" SYMWEFT_CORE_PATH="$project_root/target/debug/symweft-core" "$project_root/build/checks/mac-foundation-checks"
