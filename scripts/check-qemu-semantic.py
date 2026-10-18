#!/usr/bin/env python3
"""Read-only semantic queries against an already indexed, configured QEMU subset."""
import argparse
import collections
import json
import os
from pathlib import Path
import statistics
import time
import subprocess
import importlib.util

PROJECT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("semantic_check", PROJECT / "scripts/check-semantic.py")
protocol = importlib.util.module_from_spec(spec)
spec.loader.exec_module(protocol)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=PROJECT / "test_code/qemu")
    parser.add_argument("--commands", type=Path, default=PROJECT / "build/semantic/qemu-build/compile_commands.json")
    parser.add_argument("--cache", type=Path, default=PROJECT / "build/semantic/qemu-final/cache")
    parser.add_argument("--output", type=Path, default=PROJECT / "build/semantic/qemu-final/queries.json")
    parser.add_argument("--repeats", type=int, default=30)
    parser.add_argument("--validation", choices=["strict", "cached"], default="strict")
    args = parser.parse_args()
    assert 1 <= args.repeats <= 1000
    root = args.root.resolve()
    client = protocol.Client(root, args.cache.resolve())
    try:
        status = client.rpc("semantic.configure", {"compile_commands": str(args.commands.resolve())})
        assert status["configuration_matches"]
        assert any(u["state"] == "failed" and u["file"].endswith("system/runstate.c") for u in status["index"]["units"])
        if args.validation == "cached":
            watch = client.rpc("semantic.watch", {"enabled": True, "auto_refresh": False, "metadata_cache": True})
            assert watch["monitor"] and not watch["monitor"]["degraded"], watch
        client.rpc("document.open", {"path": "system/vl.c"})
        text = (root / "system/vl.c").read_text()
        offset = len(text[:text.index("void qemu_init(") + len("void ")].encode())
        context = client.rpc("semantic.at", {"path": "system/vl.c", "byte_offset": offset})
        entity = context["entity_id"]
        assert entity and any(s["id"] == entity and s["name"] == "qemu_init" for s in context["enclosing_functions"])
        for declaration in context["declarations"]:
            location = declaration["location"]
            span = Path(location["path"]).read_bytes()[location["offset"]:location["end_offset"]]
            assert span == b"qemu_init", span
        methods = {
            "at": ("semantic.at", {"path": "system/vl.c", "byte_offset": offset}),
            **{direction: ("semantic.relations", {"entity_id": entity, "direction": direction, "limit": 1000}) for direction in ["definitions", "incoming", "outgoing"]},
        }
        results = {}
        rss_samples = []
        for label, (method, params) in methods.items():
            start = time.perf_counter_ns()
            response = client.rpc(method, params)
            first_ms = (time.perf_counter_ns() - start) / 1e6
            samples = []
            for _ in range(args.repeats):
                start = time.perf_counter_ns()
                client.rpc(method, params)
                samples.append((time.perf_counter_ns() - start) / 1e6)
            ordered = sorted(samples)
            rss_samples.append(int(subprocess.check_output(["/bin/ps", "-o", "rss=", "-p", str(client.process.pid)]).strip()) * 1024)
            results[label] = {"response": response, "timing_ms": {"first": first_ms, "p50": statistics.median(samples), "p95": ordered[min(len(ordered) - 1, int(len(ordered) * .95))], "max": max(samples), "samples": len(samples)}}
        definitions = results["definitions"]["response"]["rows"]
        incoming = results["incoming"]["response"]["rows"]
        outgoing = results["outgoing"]["response"]["rows"]
        assert any(s["definition"] and s["location"]["path"].endswith("/system/vl.c") for s in definitions)
        assert any(c["location"]["path"].endswith("/system/main.c") and c["target"] == entity for c in incoming)
        assert outgoing and all(c["caller"] == entity for c in outgoing)
        main = (root / "system/main.c").read_text()
        main_offset = len(main[:main.index("qemu_init(argc")].encode())
        call_context = client.rpc("semantic.at", {"path": "system/main.c", "byte_offset": main_offset})
        assert call_context["entity_id"] == entity
        idle = None
        if args.validation == "cached":
            before_idle = client.rpc("semantic.progress")["watch"]
            for _ in range(8):
                time.sleep(.1)
                client.rpc("semantic.progress")
            after_idle = client.rpc("semantic.progress")["watch"]
            idle = {"polls": 10, "wait_seconds": .8, "additional_input_hashes": after_idle["fingerprints"]["hashes"] - before_idle["fingerprints"]["hashes"], "additional_configuration_hashes": after_idle["configuration_fingerprints"]["hashes"] - before_idle["configuration_fingerprints"]["hashes"], "auto_jobs": after_idle["auto_jobs"]}
            assert idle["additional_input_hashes"] == 0 and idle["additional_configuration_hashes"] == 0 and idle["auto_jobs"] == 0, idle
        report = {
            "source_root": str(root), "configured_units": status["configured_units"],
            "compiler_environment": {key: os.environ.get(key, "") for key in ["CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "SDKROOT", "MACOSX_DEPLOYMENT_TARGET", "LIBCLANG_PATH"]},
            "scope": "Already indexed subset; IPC + input hashing + SQLite + fact decoding; OS page cache not purged; excludes AST and UI rendering.",
            "idle_check": idle,
            "validation": args.validation, "watch": client.rpc("semantic.progress")["watch"],
            "core_rss_observed_bytes": rss_samples,
            "entity_name": "qemu_init", "entity_id": entity,
            "counts": {"definitions": len(definitions), "incoming": len(incoming), "outgoing": len(outgoing)},
            "outgoing_resolution": dict(collections.Counter(c["resolution"] for c in outgoing)),
            "callsite_context": call_context, "queries": results,
        }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2))
        print(json.dumps({"counts": report["counts"], "outgoing_resolution": report["outgoing_resolution"], "timing_ms": {k: v["timing_ms"] for k, v in results.items()}}, ensure_ascii=False, indent=2))
        print("PASS QEMU main.c -> qemu_init -> callees, with actual definition token range")
    finally:
        client.close()


if __name__ == "__main__":
    main()
