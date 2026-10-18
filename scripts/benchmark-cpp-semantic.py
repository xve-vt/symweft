#!/usr/bin/env python3
"""Real configured QEMU subset. Builds/dependency installation are separate."""
import argparse
import datetime
import json
import os
from pathlib import Path
import re
import subprocess

PROJECT = Path(__file__).resolve().parent.parent
DEFAULT_PATHS = [
    "system/main.c", "system/vl.c", "system/runstate.c", "system/memory.c", "system/cpus.c", "system/physmem.c",
    "qom/object.c", "qom/container.c", "qom/object_interfaces.c", "qom/qom-qobject.c",
    "qapi/visitor.c", "qapi/qobject-input-visitor.c", "qapi/qobject-output-visitor.c", "qapi/string-input-visitor.c", "qapi/string-output-visitor.c",
    "util/cutils.c", "util/crc32c.c", "util/error.c", "util/error-report.c", "util/async.c", "util/log.c", "util/module.c", "util/osdep.c", "util/path.c",
    "util/qemu-thread-posix.c", "util/qemu-timer.c", "util/qht.c", "util/rcu.c", "util/qtree.c", "hw/core/qdev.c", "target/arm/cpu.c", "target/arm/helper.c",
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=PROJECT / "test_code/qemu")
    parser.add_argument("--commands", type=Path, default=PROJECT / "build/semantic/qemu-build/compile_commands.json")
    parser.add_argument("--output", type=Path, default=PROJECT / "build/semantic" / ("benchmark-" + datetime.datetime.now().strftime("%Y%m%d-%H%M%S")))
    parser.add_argument("--path", action="append", dest="paths")
    args = parser.parse_args()
    root = args.root.resolve()
    commands = args.commands.resolve()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    assert not (output / "cache").exists(), "select a fresh benchmark output directory"
    entries = json.loads(commands.read_text())
    known = {str((Path(c["directory"]) / c["file"]).resolve()) for c in entries}
    requested = args.paths or DEFAULT_PATHS
    selected = [p for p in requested if str((root / p).resolve()) in known]
    assert selected, "no requested source files have real compile commands"
    selection = {"source_root": str(root), "commands": str(commands), "configured_commands": len(entries), "selected_paths": selected, "unavailable_requested": [p for p in requested if p not in selected], "os_page_cache": "not purged"}
    (output / "selection.json").write_text(json.dumps(selection, indent=2))
    env = dict(os.environ, SYMWEFT_CACHE_DIR=str(output / "cache"))
    summary = {"selection": selection, "compiler_environment": {key: env.get(key, "") for key in ["CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "SDKROOT", "MACOSX_DEPLOYMENT_TARGET", "LIBCLANG_PATH"]}, "phases": {}}
    for phase in ["first", "warm"]:
        with (output / (phase + ".json")).open("w") as stdout, (output / (phase + "-resource.txt")).open("w") as stderr:
            result = subprocess.run(["/usr/bin/time", "-l", str(PROJECT / "target/release/symweft-core"), "semantic-index", str(root), str(commands), *selected], env=env, stdout=stdout, stderr=stderr)
        data = json.loads((output / (phase + ".json")).read_text())
        resources = (output / (phase + "-resource.txt")).read_text()
        rss = re.search(r"(\d+)\s+maximum resident set size", resources)
        real = re.search(r"([\d.]+) real\s+([\d.]+) user\s+([\d.]+) sys", resources)
        summary["phases"][phase] = {"exit_code": result.returncode, "result": data, "resource_text": resources, "command_peak_rss_bytes": int(rss[1]) if rss else None, "process_real_seconds": float(real[1]) if real else None}
        print(phase, json.dumps(data["job"], ensure_ascii=False), flush=True)
    summary["database_bytes"] = sum(p.stat().st_size for p in (output / "cache").glob("*/a2-clang-v2/index.sqlite3"))
    (output / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2))
    print("Saved:", output)


if __name__ == "__main__":
    main()
