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
    expected_count = len(params.get("documents", [])) or 1
    assert len(version["documents"]) == expected_count
    document = next(item for item in version["documents"] if item["path"] == ack["ticket"]["path"])
    assert document["revision"] == revision
    assert all(len(item["content_key"]) == 64 for item in version["documents"])
    assert version["coverage"] == {"kind": "overlay_translation_unit", "unit_id": ack["ticket"]["unit_id"]}
    project_version = ack["project_query_version"]
    assert project_version["documents"] == version["documents"]
    assert project_version["coverage"] == {"kind": "overlay_with_persistent_index", "unit_id": ack["ticket"]["unit_id"]}
    return dict(path=path, revision=revision, job_id=ack["ticket"]["job_id"], query_version=version, project_query_version=project_version)


def project(key):
    return dict(path=key["path"], revision=key["revision"], job_id=key["job_id"], query_version=key["project_query_version"])


def check_multiple_buffers(output):
    with tempfile.TemporaryDirectory(prefix="semantic-preview-documents-", dir=output) as temp:
        root = Path(temp).resolve()
        header = '#ifdef __cplusplus\nextern "C" {\n#endif\nint first(void); int second(void);\n#define CHOSEN first\n#ifdef __cplusplus\n}\n#endif\n'
        source = '#include "api.h"\nint caller(){return CHOSEN();}\n'
        (root / "api.h").write_text(header)
        (root / "a.c").write_text('int first(void){return 1;} int second(void){return 2;}\n')
        (root / "b.cpp").write_text(source)
        (root / "c.c").write_text('#include "api.h"\nint c_caller(void){return CHOSEN();}\n')
        (root / "compile_commands.json").write_text(json.dumps([{"directory": str(root), "file": p, "arguments": ["clang++" if p.endswith("cpp") else "clang", "-c", p]} for p in ["a.c", "b.cpp", "c.c"]]))
        client = protocol.Client(root, root / "cache")
        try:
            client.configure()
            client.index()
            second = client.symbol("second")["id"]
            main_opened = client.rpc("document.open", {"path": "b.cpp"})
            header_opened = client.rpc("document.open", {"path": "api.h"})
            macro_offset = len(header[:header.index("first", header.index("#define CHOSEN"))].encode())
            header_edit = client.rpc("document.edit", {"path": "api.h", "base_revision": header_opened["analysis"]["revision"], "start_byte": macro_offset, "old_end_byte": macro_offset + 5, "new_text": "second"})
            header_revision = header_edit["analysis"]["revision"]
            main_bytes = len(source.encode())
            main_edit = client.rpc("document.edit", {"path": "b.cpp", "base_revision": main_opened["analysis"]["revision"], "start_byte": main_bytes, "old_end_byte": main_bytes, "new_text": "// unsaved main\n"})
            main_revision = main_edit["analysis"]["revision"]
            documents = [{"path": "api.h", "revision": header_revision}, {"path": "b.cpp", "revision": main_revision}]

            error = client.rpc("semantic.preview.start", {"path": "b.cpp", "revision": main_revision}, expect_error=True)
            assert error["code"] == "multiple_dirty_buffers", error
            error = client.rpc("semantic.preview", {"path": "b.cpp", "revision": main_revision}, expect_error=True)
            assert error["code"] == "multiple_dirty_buffers", error
            error = client.rpc("semantic.preview.start", {"path": "b.cpp", "revision": main_revision, "documents": [documents[1]]}, expect_error=True)
            assert error["code"] == "missing_overlay_documents", error
            error = client.rpc("semantic.preview.start", {"path": "b.cpp", "revision": main_revision, "documents": [documents[1], documents[1], documents[0]]}, expect_error=True)
            assert error["code"] == "invalid_params", error
            wrong_primary = [{"path": "b.cpp", "revision": main_revision + 1}, documents[0]]
            error = client.rpc("semantic.preview.start", {"path": "b.cpp", "revision": main_revision, "documents": wrong_primary}, expect_error=True)
            assert error["code"] == "invalid_params", error
            stale_header = [{"path": "api.h", "revision": header_revision - 1}, documents[1]]
            error = client.rpc("semantic.preview.start", {"path": "b.cpp", "revision": main_revision, "documents": stale_header}, expect_error=True)
            assert error["code"] == "stale_revision", error
            too_many = documents + [{"path": "extra%d.h" % index, "revision": 1} for index in range(15)]
            error = client.rpc("semantic.preview.start", {"path": "b.cpp", "revision": main_revision, "documents": too_many}, expect_error=True)
            assert error["code"] == "overlay_budget", error

            ack = client.rpc("semantic.preview.start", {"path": "b.cpp", "revision": main_revision, "documents": documents})
            assert ack["queued"] == True or ack["worker_active"] == True
            assert ack["queue_len"] >= 1, ack
            assert any("c.c" in u for u in ack["queued_units"]), ack
            key = dict(path="b.cpp", revision=main_revision, job_id=ack["ticket"]["job_id"], query_version=ack["query_version"], project_query_version=ack["project_query_version"])
            status = wait(client, key)
            revisions = {(Path(item["path"]).name, item["revision"]) for item in key["query_version"]["documents"]}
            assert revisions == {("api.h", header_revision), ("b.cpp", main_revision)}, revisions
            coverage = status["ticket"]["includer_coverage"]
            assert coverage["scope"] == "partial", coverage
            assert any("c.c" in str(s) for s in coverage["stale_includers"]), coverage
            assert any("b.cpp" in str(s) for s in coverage["updated_includers"]), coverage
            assert "partial includer coverage" in status["completeness"], status["completeness"]
            assert "project not fully updated" in status["completeness"], status["completeness"]
            offset = len(source[:source.index("CHOSEN")].encode())
            context = client.rpc("semantic.preview.at", dict(key, byte_offset=offset))
            assert any(row["name"] == "second" and row["target"] == second for row in context["references"]), context
            project_rels = client.rpc("semantic.project.relations", dict(project(key), entity_id=second, direction="definitions"))
            definitions = project_rels["rows"]
            assert any(row["main_file"].endswith("a.c") and row["evidence"] == "clang_semantic" for row in definitions), definitions
            assert project_rels["includer_coverage"]["scope"] == "partial", project_rels
            assert "partial includer coverage" in project_rels["completeness"], project_rels
            assert (root / "api.h").read_text() == header
            assert (root / "b.cpp").read_text() == source

            (root / "unindexed.h").write_text("int unindexed_fn(void);\n")
            unindexed_open = client.rpc("document.open", {"path": "unindexed.h"})
            unindexed_rev = unindexed_open["analysis"]["revision"]
            unindexed_err = client.rpc("semantic.preview.start", {"path": "unindexed.h", "revision": unindexed_rev}, expect_error=True)
            assert "no known includer records" in unindexed_err["message"], unindexed_err
        finally:
            client.close()
    print("PASS main/header buffers compile together with complete revisions, dirty coverage and budget rejection")
    return {"documents": 2, "target": "second"}


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
    results["multiple_buffers"] = check_multiple_buffers(output)
    (output / "semantic-preview.json").write_text(json.dumps(results, indent=2))
    print("All asynchronous semantic preview checks passed.")


if __name__ == "__main__":
    main()
