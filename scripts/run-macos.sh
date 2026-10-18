#!/usr/bin/env bash
set -euo pipefail
project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bash "$project_root/scripts/build-macos.sh"
workspace_path="${1:-$project_root}"
open "$project_root/build/macos/Symweft.app" --args --workspace "$workspace_path"
