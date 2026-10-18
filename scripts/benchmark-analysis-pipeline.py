#!/usr/bin/env python3
"""PERF-01: QEMU pipeline stages and owned Core/native process resources."""
import argparse
import datetime
import importlib.util
import hashlib
import json
import os
from pathlib import Path
import platform
import sqlite3
import statistics
import subprocess
import time

from resource_probe import ResourceSampler, summarize

PROJECT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("protocol", PROJECT / "scripts/check-semantic.py")
protocol = importlib.util.module_from_spec(spec)
spec.loader.exec_module(protocol)


def timings(samples):
    values = sorted(samples)
    return dict(samples=len(values), p50_ms=statistics.median(values),
                p95_ms=values[min(len(values) - 1, int(len(values) * .95))], max_ms=max(values))


def backup_cache(source, target):
    paths = list(source.rglob("index.sqlite3"))
    assert paths, "no index databases to back up"
    for path in paths:
        dest = target / path.relative_to(source)
        dest.parent.mkdir(parents=True, exist_ok=True)
        with sqlite3.connect(str(path)) as src, sqlite3.connect(str(dest)) as dst:
            src.backup(dst)


def counters(status):
    watch = status["watch"]
    return dict(input_hashes=watch["fingerprints"]["hashes"],
                configuration_hashes=watch["configuration_fingerprints"]["hashes"],
                auto_jobs=watch["auto_jobs"])


def measured_rpc(client, method, params):
    begin = time.perf_counter_ns()
    result = client.rpc(method, params)
    return result, (time.perf_counter_ns() - begin) / 1e6


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=PROJECT / "build/performance" / datetime.datetime.now().strftime("%Y%m%d-%H%M%S"))
    parser.add_argument("--interval-ms", type=int, default=50)
    parser.add_argument("--idle-seconds", type=float, default=8)
    parser.add_argument("--control-pairs", type=int, default=3)
    parser.add_argument("--skip-ui", action="store_true", help="partial check only; does not complete PERF-01")
    args = parser.parse_args()
    assert 6 <= args.idle_seconds <= 30
    assert 1 <= args.control_pairs <= 6
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    assert not (output / "cache").exists(), "select a fresh output directory"
    root = PROJECT / "test_code/qemu"
    commands = PROJECT / "build/semantic/qemu-build/compile_commands.json"
    app = PROJECT / "build/macos/Symweft.app/Contents/MacOS/Symweft"
    assert commands.exists()
    assert args.skip_ui or app.exists(), "build the release macOS app first"
    os.environ["SYMWEFT_PROFILE"] = "1"  # Applies only to this benchmark process and its children.
    report = dict(schema=1, task="PERF-01", platform=platform.platform(), machine=platform.machine(),
        source_root=str(root), commands=str(commands),
        qemu_commit=subprocess.check_output(["git", "-C", str(root), "rev-parse", "HEAD"], text=True).strip(),
        scope="2 configured TUs (system/main.c, system/vl.c); 1 unsaved TU; warm OS caches; no full QEMU A2",
        compiler_environment={k: os.environ.get(k, "") for k in ["CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "SDKROOT", "MACOSX_DEPLOYMENT_TARGET", "LIBCLANG_PATH"]},
        instrumentation=dict(enabled=True, process_interval_ms=args.interval_ms, profile_unit_cap=64,
            serializer="unchanged streaming JSON; timed forwarded writer; probe overhead included",
            gpu="not measured; no GPU graph implementation yet", external_window_server="not attributed"))
    source = root / "system/vl.c"
    original = source.read_bytes()
    text = original.decode()
    path = "system/vl.c"
    cache = output / "cache"
    client = protocol.Client(root, cache)
    sampler = ResourceSampler([(client.process.pid, "core")], output / "core-resources.jsonl", args.interval_ms).start()
    try:
        configured = client.rpc("semantic.configure", {"compile_commands": str(commands)})
        report["configuration"] = {key: configured[key] for key in ["configuration", "configured_units", "provider"]}
        client.rpc("semantic.watch", {"enabled": True, "auto_refresh": False, "metadata_cache": True})
        report["index"] = {}
        for phase in ["index_first", "index_reuse"]:
            sampler.set_phase(phase)
            started = time.perf_counter_ns()
            job = client.index(["system/main.c", path])
            report["index"][phase] = dict(wall_ms=(time.perf_counter_ns() - started) / 1e6, job=job)
            assert "profile" in job
            if phase == "index_first":
                assert job["analyzed"] == 2
            else:
                assert job["reused"] == 2 and job["analyzed"] == 0
        sampler.set_phase("prepare_preview")
        client.rpc("index.flush")
        backup_cache(cache, output / "native-cache")
        opened = client.rpc("document.open", {"path": path})
        edited = client.rpc("document.edit", dict(path=path, base_revision=opened["analysis"]["revision"],
            start_byte=len(original), old_end_byte=len(original), new_text="\n/* PERF-01 unsaved benchmark only. */\n"))
        sampler.set_phase("preview")
        start = time.perf_counter_ns()
        ack, ack_ms = measured_rpc(client, "semantic.preview.start", dict(path=path, revision=edited["analysis"]["revision"]))
        key = dict(path=path, revision=edited["analysis"]["revision"], job_id=ack["ticket"]["job_id"])
        reads = []
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            _, latency = measured_rpc(client, "document.read", {"path": "system/main.c"})
            reads.append(latency)
            preview = client.rpc("semantic.preview.progress", key)
            if preview["state"] not in ["running", "queued"]:
                break
            time.sleep(.02)
        assert preview["state"] == "ready", preview
        report["preview"] = dict(ack_ms=ack_ms, ready_wall_ms=(time.perf_counter_ns() - start) / 1e6,
            source_read_during_analysis=timings(reads), result=preview)
        offset = len(text[:text.index("void qemu_init(") + len("void ")].encode())
        context = client.rpc("semantic.preview.at", dict(key, byte_offset=offset))
        assert context["entity_id"]
        sampler.set_phase("queries")
        report["queries"] = {}
        for name, method, params in [
            ("overlay_at", "semantic.preview.at", dict(key, byte_offset=offset)),
            ("overlay_outgoing", "semantic.preview.relations", dict(key, entity_id=context["entity_id"], direction="outgoing", limit=1000)),
            ("disk_incoming", "semantic.relations", dict(entity_id=context["entity_id"], direction="incoming", limit=1000)),
        ]:
            values = []
            for _ in range(30):
                response, latency = measured_rpc(client, method, params)
                values.append(latency)
            report["queries"][name] = timings(values)
            if name == "overlay_outgoing":
                assert len(response["rows"]) == 405
            if name == "disk_incoming":
                assert len(response["rows"]) == 1
            if "rows" in response:
                report["queries"][name]["rows"] = len(response["rows"])
            else:
                report["queries"][name]["entity_found"] = bool(response.get("entity_id"))
        before = counters(client.rpc("semantic.progress"))
        sampler.set_phase("core_idle")
        time.sleep(args.idle_seconds)
        sampler.set_phase("core_cleanup")
        after = counters(client.rpc("semantic.progress"))
        assert after == before, (before, after)
        report["core_idle"] = dict(seconds=args.idle_seconds, before=before, after=after, rpc_during_idle=0)
        client.rpc("semantic.preview.cancel", {"job_id": key["job_id"]})
    finally:
        client.close()
        report["core_sampler"] = sampler.stop()
    samples = list(sampler.samples)
    report["core_phases"] = {name: summarize(samples, phase=name) for name in dict.fromkeys(s["phase"] for s in samples)}
    assert "compiler_worker" in report["core_sampler"]["summary"]["roles"], "worker shorter than sampler resolution or unavailable"
    assert source.read_bytes() == original
    report["source_unchanged"] = True
    report["database_bytes"] = sum(p.stat().st_size for p in cache.rglob("index.sqlite3"))
    # The streaming writer probe times many small writes. Quantify its observer
    # effect with interleaved off/on previews, not an assumed zero-overhead claim.
    backup_cache(cache, output / "control-cache")
    controls = []
    for repeat in range(args.control_pairs):
        for enabled in [False,True]:
            os.environ["SYMWEFT_PROFILE"] = "1" if enabled else "0"
            control = protocol.Client(root,output / "control-cache")
            try:
                control.rpc("semantic.configure",{"compile_commands":str(commands)})
                control.rpc("semantic.watch",{"enabled":True,"auto_refresh":False,"metadata_cache":True})
                opened = control.rpc("document.open",{"path":path})
                edited = control.rpc("document.edit",dict(path=path,base_revision=opened["analysis"]["revision"],
                    start_byte=len(original),old_end_byte=len(original),new_text="\n/* PERF-01 control. */\n"))
                started = time.perf_counter_ns()
                ack = control.rpc("semantic.preview.start",dict(path=path,revision=edited["analysis"]["revision"]))
                key = dict(path=path,revision=edited["analysis"]["revision"],job_id=ack["ticket"]["job_id"])
                deadline = time.monotonic()+15
                while time.monotonic()<deadline:
                    result = control.rpc("semantic.preview.progress",key)
                    if result["state"] not in ["queued","running"]:break
                    time.sleep(.01)
                assert result["state"]=="ready",result
                controls.append(dict(repeat=repeat,enabled=enabled,backend_wall_ms=result["wall_ms"],
                    ready_wall_ms=(time.perf_counter_ns()-started)/1e6,
                    profile_present=result["profile"] is not None))
                assert controls[-1]["profile_present"]==enabled
            finally:control.close()
    os.environ["SYMWEFT_PROFILE"]="1"
    report["probe_controls"] = dict(samples=controls,
        off=timings([v["backend_wall_ms"] for v in controls if not v["enabled"]]),
        on=timings([v["backend_wall_ms"] for v in controls if v["enabled"]]),
        scope=f"{args.control_pairs} interleaved pairs, same disk inputs, no OS cache purge; preview only; not a stable tail-latency estimate")
    if not args.skip_ui:
        ready = output / "native-ready"
        native = output / "native-idle.json"
        with (output / "native.stdout").open("w") as stdout, (output / "native.stderr").open("w") as stderr:
            process = subprocess.Popen([str(app), "--workspace", str(root), "--file", path, "--compile-commands", str(commands),
                "--inspect-symbol", "qemu_init", "--perf-idle-to", str(native), "--perf-refresh-indexed", "true", "--perf-idle-seconds", str(args.idle_seconds),
                "--qa-ready-to", str(ready)], stdout=stdout, stderr=stderr,
                env=dict(os.environ, SYMWEFT_CACHE_DIR=str(output / "native-cache")))
            ui_sampler = ResourceSampler([(process.pid, "ui")], output / "native-resources.jsonl", args.interval_ms).start()
            try:
                process.wait(timeout=60)
                assert process.returncode == 0, (output / "native.stderr").read_text()
                report["native_idle"] = json.loads(native.read_text())
                assert report["native_idle"]["before"] == report["native_idle"]["after"], report["native_idle"]
                assert report["native_idle"]["current_function"] == "qemu_init"
            finally:
                if process.poll() is None:
                    process.kill(); process.wait()
                report["native_sampler"] = ui_sampler.stop()
        bounds = (report["native_idle"]["start_unix_ns"], report["native_idle"]["end_unix_ns"])
        report["native_idle_resources"] = summarize(list(ui_sampler.samples), unix_bounds=bounds)
        assert set(report["native_idle_resources"]["roles"]) == {"ui", "core"}, report["native_idle_resources"]
        activity = report["native_idle"]["activity"]
        assert activity["analyzed_units"] == 2
        report["native_analysis_resources"] = summarize(list(ui_sampler.samples),unix_bounds=(activity["start_unix_ns"],activity["end_unix_ns"]))
        assert "compiler_worker" in report["native_analysis_resources"]["roles"], report["native_analysis_resources"]
    def checksum(file):
        digest=hashlib.sha256()
        with file.open("rb") as stream:
            for chunk in iter(lambda:stream.read(65536),b""):digest.update(chunk)
        return digest.hexdigest()
    report["input_manifest"] = {str(p):dict(bytes=p.stat().st_size,sha256=checksum(p)) for p in
        [commands,source,protocol.CORE,app,app.parent/"symweft-core"] if p.exists()}
    (output / "pipeline.json").write_text(json.dumps(report, indent=2) + "\n")
    print("Saved:", output / "pipeline.json")
    print(json.dumps({"index_ms":{k:v["wall_ms"] for k,v in report["index"].items()},
        "preview_ms":report["preview"]["ready_wall_ms"], "queries":report["queries"],
        "core_tree_rss_sampled_max_mib":report["core_sampler"]["summary"]["tree_rss_sampled_max_bytes"] / 1048576,
        "native_idle_resources":report.get("native_idle_resources")}, indent=2))


if __name__ == "__main__":
    main()
