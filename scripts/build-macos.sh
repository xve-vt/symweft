#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
configuration="${SYMWEFT_BUILD_CONFIG:-debug}"
if [[ "$configuration" != "debug" && "$configuration" != "release" ]]; then
    echo "SYMWEFT_BUILD_CONFIG must be debug or release." >&2
    exit 2
fi
rust_options=(--locked --package symweft-core --manifest-path "$project_root/Cargo.toml" --target-dir "$project_root/target")
if [[ "$configuration" == "release" ]]; then rust_options+=(--release); fi
if [[ "${SYMWEFT_OFFLINE:-1}" == "1" ]]; then rust_options+=(--offline); fi
cargo build "${rust_options[@]}"

swift_options=(--package-path "$project_root/apps/macos" --scratch-path "$project_root/build/swift" --configuration "$configuration" --disable-sandbox)
swift build "${swift_options[@]}"
swift_binary_path="$(swift build "${swift_options[@]}" --show-bin-path)"

app_path="$project_root/build/macos/Symweft.app"
mkdir -p "$app_path/Contents/MacOS"
cp "$project_root/apps/macos/Info.plist" "$app_path/Contents/Info.plist"
cp "$swift_binary_path/Symweft" "$app_path/Contents/MacOS/Symweft"
cp "$project_root/target/$configuration/symweft-core" "$app_path/Contents/MacOS/symweft-core"
codesign --force --sign - --timestamp=none "$app_path/Contents/MacOS/symweft-core"
codesign --force --sign - --timestamp=none "$app_path"
echo "Built: $app_path"
