#!/usr/bin/env python3
"""Production async overlay lifecycle, latest revision, queries and isolation."""
import copy
import importlib.util
import json
import statistics
import tempfile
import time
from pathlib import Path

PROJECT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("protocol", PROJECT / "scripts/check-semantic.py")
protocol = importlib.util.module_from_spec(spec)
spec.loader.exec_module(protocol)


def wait(client, key, expected="ready"):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        value = client.rpc("semantic.preview.progress", key)
        if value["state"] not in ["queued", "running"]:
            assert value["state"] == expected, value
            return value
        time.sleep(.01)
    raise AssertionError("preview deadline")


def start(client, path, revision, **params):
    ack = client.rpc("semantic.preview.start", dict(path=path, revision=revision, **params))
    assert ack["overlay"] and not ack["persisted"]
    version = ack["query_version"]
    assert version["contract"] == "a2-query-v1"
    assert len(version["documents"]) == 1
    document = version["documents"][0]
    assert document["path"] == ack["ticket"]["path"] and document["revision"] == revision
    assert len(document["content_key"]) == 64
    assert version["coverage"] == {"kind": "overlay_translation_unit", "unit_id": ack["ticket"]["unit_id"]}
    project_version = ack["project_query_version"]
    assert project_version["documents"] == version["documents"]
    assert project_version["coverage"] == {"kind": "overlay_with_persistent_index", "unit_id": ack["ticket"]["unit_id"]}
    return dict(path=path, revision=revision, job_id=ack["ticket"]["job_id"], query_version=version, project_query_version=project_version)


def project(key):
    return dict(path=key["path"], revision=key["revision"], job_id=key["job_id"], query_version=key["project_query_version"])


def main():
    output = PROJECT / "build/checks"
    output.mkdir(parents=True, exist_ok=True)
    results = {}
    with tempfile.TemporaryDirectory(prefix="semantic-preview-", dir=output) as temp:
        root = Path(temp).resolve()
        source = '// UTF-8 织源\n#include "api.h"\nint caller(){return first();}\n'
        header = '#ifdef __cplusplus\nextern "C" {\n#endif\nint first(void); int second(void);\n#ifdef __cplusplus\n}\n#endif\n'
        (root / "api.h").write_text(header)
        (root / "a.c").write_text('int first(void){return 1;} int second(void){return 2;}\n')
        (root / "b.cpp").write_text(source)
        (root / "c.c").write_text('#include "api.h"\nint other(void){return first();}\n')
        commands = [{"directory": str(root), "file": p, "arguments": ["clang++" if p.endswith("cpp") else "clang", "-c", p]} for p in ["a.c", "b.cpp", "c.c"]]
        (root / "compile_commands.json").write_text(json.dumps(commands))
        client = protocol.Client(root, root / "cache")
        try:
            client.configure()
            client.index()
            first = client.symbol("first")["id"]
            second = client.symbol("second")["id"]
            opened = client.rpc("document.open", {"path": "b.cpp"})
            revision = opened["analysis"]["revision"]
            offset = len(source[:source.index("first()")].encode())
            edit = client.rpc("document.edit", {"path": "b.cpp", "base_revision": revision, "start_byte": offset, "old_end_byte": offset + 5, "new_text": "second"})
            revision = edit["analysis"]["revision"]
            before = client.rpc("semantic.status")["index"]
            key = start(client, "b.cpp", revision)
            responsive = []
            for _ in range(8):
                begin = time.perf_counter_ns()
                client.rpc("document.read", {"path": "a.c"})
                responsive.append((time.perf_counter_ns() - begin) / 1e6)
            results["initial"] = wait(client, key)
            context = client.rpc("semantic.preview.at", dict(key, byte_offset=offset))
            assert context["entity_id"] == second and context["query_version"] == key["query_version"], context
            caller = context["enclosing_functions"][0]["id"]
            for direction, entity, count in [("outgoing", caller, 1), ("incoming", second, 1), ("references", second, 1), ("definitions", caller, 1), ("definitions", second, 0)]:
                relation = client.rpc("semantic.preview.relations", dict(key, entity_id=entity, direction=direction))
                rows = relation["rows"]
                assert relation["query_version"] == key["query_version"]
                assert len(rows) == count, (direction, rows)
                assert all(r["freshness"] == "overlay" and r["revision"] == revision for r in rows)
            wrong_version = copy.deepcopy(key["query_version"])
            wrong_version["documents"][0]["revision"] += 1
            error = client.rpc("semantic.preview.at", dict(key, query_version=wrong_version, byte_offset=offset), expect_error=True)
            assert error["code"] == "stale_query_version", error

            project_context = client.rpc("semantic.project.at", dict(project(key), byte_offset=offset))
            assert project_context["entity_id"] == second
            assert project_context["query_version"] == key["project_query_version"]
            assert project_context["overlay_unit_id"] == key["project_query_version"]["coverage"]["unit_id"]
            first_incoming = client.rpc("semantic.project.relations", dict(project(key), entity_id=first, direction="incoming"))
            assert len(first_incoming["rows"]) == 1, first_incoming
            assert first_incoming["rows"][0]["main_file"].endswith("c.c")
            assert first_incoming["rows"][0]["evidence"] == "clang_semantic"
            second_incoming = client.rpc("semantic.project.relations", dict(project(key), entity_id=second, direction="incoming"))
            assert len(second_incoming["rows"]) == 1 and second_incoming["rows"][0]["target"] == second
            assert second_incoming["rows"][0]["evidence"] == "clang_semantic_overlay"
            second_definitions = client.rpc("semantic.project.relations", dict(project(key), entity_id=second, direction="definitions"))
            assert len(second_definitions["rows"]) == 1
            assert second_definitions["rows"][0]["main_file"].endswith("a.c")
            assert second_definitions["rows"][0]["evidence"] == "clang_semantic"
            outgoing = client.rpc("semantic.project.relations", dict(project(key), entity_id=caller, direction="outgoing"))
            assert len(outgoing["rows"]) == 1 and outgoing["rows"][0]["target"] == second
            declarations = client.rpc("semantic.project.relations", dict(project(key), entity_id=second, direction="declarations"))["rows"]
            assert any(row["evidence"] == "clang_semantic" for row in declarations)
            assert any(row["evidence"] == "clang_semantic_overlay" for row in declarations)
            references = client.rpc("semantic.project.relations", dict(project(key), entity_id=second, direction="references"))["rows"]
            assert len(references) == 1 and references[0]["evidence"] == "clang_semantic_overlay"
            merged_rows = first_incoming["rows"] + second_incoming["rows"] + second_definitions["rows"] + outgoing["rows"] + declarations + references
            assert all(row["unit_id"] and row["main_file"] and row["freshness"] for row in merged_rows)
            assert all(row["evidence"] in ["clang_semantic", "clang_semantic_overlay"] for row in merged_rows)
            missing_version = project(key)
            del missing_version["query_version"]
            error = client.rpc("semantic.project.relations", dict(missing_version, entity_id=first, direction="incoming"), expect_error=True)
            assert error["code"] == "invalid_params", error
            error = client.rpc("semantic.project.relations", dict(project(key), query_version=key["query_version"], entity_id=first, direction="incoming"), expect_error=True)
            assert error["code"] == "stale_query_version", error
            results["project"] = {"first_incoming": len(first_incoming["rows"]), "second_incoming": len(second_incoming["rows"]), "declarations": len(declarations)}
            print("PASS single-overlay project merge masks old edges and retains other TU definitions/callers and evidence")

            assert not client.relations(second, "incoming"), "overlay must not leak into disk relations"
            disk_first = client.relations(first, "incoming")
            assert len(disk_first) == 1 and disk_first[0]["main_file"].endswith("c.c"), disk_first
            assert client.rpc("semantic.status")["index"] == before, "preview must not publish persistent facts"
            assert (root / "b.cpp").read_text() == source
            results["read_during_preview_ms"] = {"p50": statistics.median(responsive), "max": max(responsive)}
            print("PASS async launch, versioned overlay relations, stale document-set rejection and disk isolation")

            # A completed old revision and a running superseded request are both rejected.
            edit = client.rpc("document.edit", {"path": "b.cpp", "base_revision": revision, "start_byte": offset, "old_end_byte": offset + 6, "new_text": "first"})
            revision = edit["analysis"]["revision"]
            assert client.rpc("semantic.preview.at", dict(key, byte_offset=offset), expect_error=True)["code"] == "stale_revision"
            old_key = start(client, "b.cpp", revision)
            edit = client.rpc("document.edit", {"path": "b.cpp", "base_revision": revision, "start_byte": offset, "old_end_byte": offset + 5, "new_text": "second"})
            revision = edit["analysis"]["revision"]
            key = start(client, "b.cpp", revision)
            client.rpc("semantic.preview.cancel", {"job_id": old_key["job_id"]}, expect_error=True)
            results["latest"] = wait(client, key)
            assert client.rpc("semantic.preview.at", dict(key, byte_offset=offset))["entity_id"] == second
            client.rpc("semantic.preview.at", dict(key, byte_offset=offset, revision=revision + 1), expect_error=True)
            client.rpc("semantic.preview.at", dict(key, byte_offset=11), expect_error=True)  # split UTF-8 character
            print("PASS newest revision wins, stale tickets/cancellation and invalid UTF-8 offsets rejected")

            (root / "api.h").write_text(header + '// modified\n')
            assert client.rpc("semantic.preview.progress", key)["state"] == "stale"
            client.rpc("semantic.preview.relations", dict(key, entity_id=caller, direction="outgoing"), expect_error=True)
            key = start(client, "b.cpp", revision)
            wait(client, key)
            client.rpc("document.close", {"path": "b.cpp"})
            assert client.rpc("semantic.progress")["preview"]["charged_bytes"] == 0
            client.rpc("semantic.preview.progress", key, expect_error=True)
            print("PASS strict dependency validation and document close discard transient facts")

            opened = client.rpc("document.open", {"path": "b.cpp"})
            revision = opened["analysis"]["revision"]
            key = start(client, "b.cpp", revision)
            client.rpc("semantic.preview.cancel", {"job_id": key["job_id"]})
            assert client.rpc("semantic.preview.progress", key)["state"] == "cancelled"
            key = start(client, "b.cpp", revision)
            wait(client, key)
            client.rpc("cache.pressure", {"level": "critical", "now_ms": 1})
            assert client.rpc("semantic.progress")["preview"]["charged_bytes"] == 0
            client.rpc("semantic.preview.start", {"path": "b.cpp", "revision": revision}, expect_error=True)
            client.rpc("cache.pressure", {"level": "normal", "now_ms": 2})
            key = start(client, "b.cpp", revision)
            wait(client, key)
            (root / "compile_commands.json").write_text(json.dumps(commands) + "\n")
            assert client.rpc("semantic.preview.progress", key)["state"] == "stale"
            client.rpc("semantic.preview.cancel", {"job_id": key["job_id"]})
            client.configure()
            new_key = start(client, "b.cpp", revision)
            assert new_key["job_id"] != key["job_id"]
            client.rpc("semantic.preview.cancel", {"job_id": key["job_id"]}, expect_error=True)
            wait(client, new_key)
            print("PASS cancellation, critical pressure eviction/resume and changed configuration")
            client.rpc("semantic.watch", {"enabled": True, "auto_refresh": False, "metadata_cache": True})
            key = start(client, "b.cpp", revision)
            wait(client, key)
            (root / "new-search-candidate.h").write_text("// include search structure changed\n")
            deadline = time.monotonic() + 6
            while time.monotonic() < deadline:
                live = client.rpc("semantic.progress")
                if live["preview"]["state"] == "stale" and live["watch"]["monitor"]["events"]:
                    break
                time.sleep(.02)
            assert live["preview"]["state"] == "stale", live
            assert live["watch"]["monitor"]["events"] > 0, live
            assert live["preview"]["charged_bytes"] == 0
            print("PASS real native structural event invalidates the retained preview")
            invalid = client.rpc("document.edit", {"path": "b.cpp", "base_revision": revision,
                "start_byte": 0, "old_end_byte": len(source.encode()), "new_text": "int broken( { }\n"})
            revision = invalid["analysis"]["revision"]
            key = start(client, "b.cpp", revision)
            failed = wait(client, key, expected="failed")
            assert failed["diagnostics"] and any(d["severity"] >= 3 for d in failed["diagnostics"]), failed
            client.rpc("semantic.preview.at", dict(key, byte_offset=0), expect_error=True)
            a_opened = client.rpc("document.open", {"path": "a.c"})
            a_bytes = len((root / "a.c").read_bytes())
            client.rpc("document.edit", {"path": "a.c", "base_revision": a_opened["analysis"]["revision"],
                "start_byte": a_bytes, "old_end_byte": a_bytes, "new_text": "// unsaved\n"})
            error = client.rpc("semantic.preview.start", {"path": "b.cpp", "revision": revision}, expect_error=True)
            assert error["code"] == "multiple_dirty_buffers", error
            print("PASS compiler diagnostics without partial facts and explicit multiple-dirty-buffer boundary")
        finally:
            client.close()
    (output / "semantic-preview.json").write_text(json.dumps(results, indent=2))
    print("All asynchronous semantic preview checks passed.")


if __name__ == "__main__":
    main()
