#!/usr/bin/env python3
"""Unsaved QEMU TU preview, source-read responsiveness and bounded query results."""
import importlib.util
import json
import os
import sqlite3
import statistics
import subprocess
import time
from pathlib import Path

PROJECT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("protocol", PROJECT / "scripts/check-semantic.py")
protocol = importlib.util.module_from_spec(spec)
spec.loader.exec_module(protocol)


def timing(samples):
    values = sorted(samples)
    return {"samples": len(values), "p50_ms": statistics.median(values),
            "p95_ms": values[min(len(values) - 1, int(len(values) * .95))], "max_ms": max(values)}


def main():
    output = PROJECT / "build/semantic/qemu-preview"
    output.mkdir(parents=True, exist_ok=True)
    cache = output / "cache"
    baseline = PROJECT / "build/semantic/qemu-realtime/cache"
    databases = list(baseline.rglob("index.sqlite3"))
    if not databases:
        raise SystemExit("Missing existing QEMU semantic baseline: " + str(baseline))
    for path in databases:
        dest = cache / path.relative_to(baseline)
        dest.parent.mkdir(parents=True, exist_ok=True)
        with sqlite3.connect(str(path)) as source, sqlite3.connect(str(dest)) as target:
            source.backup(target)
    root = PROJECT / "test_code/qemu"
    path = "system/vl.c"
    content = (root / path).read_text()
    original_bytes = (root / path).read_bytes()
    commands = PROJECT / "build/semantic/qemu-build/compile_commands.json"
    client = protocol.Client(root, cache)
    rss_samples = []
    def rss():
        rss_samples.append(int(subprocess.check_output(["/bin/ps", "-o", "rss=", "-p", str(client.process.pid)]).strip()) * 1024)
    try:
        configured = client.rpc("semantic.configure", {"compile_commands": str(commands)})
        assert configured["incompatible_provider_units"] == 0, "use the same compiler environment as the baseline"
        client.rpc("semantic.watch", {"enabled": True, "auto_refresh": False, "metadata_cache": True})
        opened = client.rpc("document.open", {"path": path})
        revision = opened["analysis"]["revision"]
        edit = client.rpc("document.edit", {"path": path, "base_revision": revision,
            "start_byte": len(original_bytes), "old_end_byte": len(original_bytes),
            "new_text": "\n/* Symweft unsaved preview benchmark; source stays unchanged. */\n"})
        revision = edit["analysis"]["revision"]
        before = client.rpc("semantic.status")["index"]
        begin = time.perf_counter_ns()
        ack = client.rpc("semantic.preview.start", {"path": path, "revision": revision})
        ack_ms = (time.perf_counter_ns() - begin) / 1e6
        key = {"path": path, "revision": revision, "job_id": ack["ticket"]["job_id"]}
        reads = []
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            read_start = time.perf_counter_ns()
            client.rpc("document.read", {"path": "system/main.c"})
            reads.append((time.perf_counter_ns() - read_start) / 1e6)
            progress = client.rpc("semantic.preview.progress", key)
            rss()
            if progress["state"] not in ["queued", "running"]:
                break
            time.sleep(.02)
        wall_ms = (time.perf_counter_ns() - begin) / 1e6
        assert progress["state"] == "ready", progress
        offset = len(content[:content.index("void qemu_init(") + len("void ")].encode())
        context = client.rpc("semantic.preview.at", dict(key, byte_offset=offset))
        entity = context["entity_id"]
        assert entity and context["enclosing_functions"][0]["name"] == "qemu_init", context
        result = {}
        for label, method, params in [
            ("at", "semantic.preview.at", dict(key, byte_offset=offset)),
            ("outgoing", "semantic.preview.relations", dict(key, entity_id=entity, direction="outgoing", limit=1000)),
        ]:
            samples = []
            for _ in range(30):
                start = time.perf_counter_ns()
                response = client.rpc(method, params)
                samples.append((time.perf_counter_ns() - start) / 1e6)
            result[label] = timing(samples)
            if label == "outgoing":
                assert len(response["rows"]) == 405, len(response["rows"])
                result[label]["rows"] = len(response["rows"])
        rss()
        assert client.rpc("semantic.status")["index"] == before
        after = client.rpc("semantic.preview.cancel", {"job_id": key["job_id"]})
        assert after["charged_bytes"] == 0
        assert (root / path).read_bytes() == original_bytes
        report = {"root": str(root), "commands": str(commands), "configured_units": configured["configured_units"],
            "scope": "one unsaved TU system/vl.c; append comment; warm OS caches; no full-project AST analysis",
            "compiler_environment": {k: os.environ.get(k, "") for k in ["CPATH", "SDKROOT", "LIBCLANG_PATH"]},
            "start_ack_ms": ack_ms, "ready_wall_ms": wall_ms, "preview": progress,
            "source_read_during_preview": timing(reads), "queries": result,
            "core_rss_observed_bytes": {"min": min(rss_samples), "max": max(rss_samples), "samples": len(rss_samples)},
            "after_cancel": after, "disk_source_unchanged": True, "disk_index_not_published": True}
        (output / "preview.json").write_text(json.dumps(report, indent=2))
        print(json.dumps({k: report[k] for k in ["start_ack_ms", "ready_wall_ms", "source_read_during_preview", "queries", "core_rss_observed_bytes"]}, indent=2))
        print("retained bytes:", progress["charged_bytes"])
    finally:
        client.close()


if __name__ == "__main__":
    main()
