#!/usr/bin/env python3
"""Exercise production stdio + isolated compiler + persistence, including edits."""
import copy
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import time

PROJECT = Path(__file__).resolve().parent.parent
CORE = Path(os.environ.get("SYMWEFT_CORE_PATH", PROJECT / "target/release/symweft-core"))


class Client:
    def __init__(self, root, cache):
        self.root = root
        self.seq = 0
        self.process = subprocess.Popen(
            [str(CORE), "serve"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True,
            env=dict(os.environ, SYMWEFT_CACHE_DIR=str(cache)))
        self.rpc("initialize")
        self.rpc("workspace.open", {"root": str(root)})

    def rpc(self, method, params=None, expect_error=False):
        self.seq += 1
        self.process.stdin.write(json.dumps({"id": self.seq, "method": method, "params": params or {}}) + "\n")
        self.process.stdin.flush()
        assert select.select([self.process.stdout], [], [], 15)[0], "protocol timeout: " + method
        reply = json.loads(self.process.stdout.readline())
        assert reply["id"] == self.seq, reply
        if expect_error:
            assert "error" in reply, reply
            return reply["error"]
        assert "error" not in reply, reply
        return reply["result"]

    def configure(self):
        return self.rpc("semantic.configure", {"compile_commands": str(self.root / "compile_commands.json")})

    def wait(self, success=True):
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            result = self.rpc("semantic.progress")
            if result["job"]["state"] != "running":
                if success:
                    assert result["job"]["state"] == "complete", result
                return result["job"]
            time.sleep(0.025)
        raise AssertionError("semantic job deadline")

    def index(self, paths=None, success=True):
        ack = self.rpc("semantic.index", {"paths": paths or []})
        assert ack["started"], ack
        return self.wait(success)

    def symbol(self, name, definition=True):
        rows = self.rpc("semantic.symbols", {"name": name})["rows"]
        return next(s for s in rows if s["definition"] == definition)

    def relations(self, entity, direction):
        return self.rpc("semantic.relations", {"entity_id": entity, "direction": direction})["rows"]

    def close(self):
        self.process.stdin.close()
        self.process.wait(timeout=5)
        errors = self.process.stderr.read()
        self.process.stdout.close()
        self.process.stderr.close()
        assert self.process.returncode == 0, errors


def main():
    output = PROJECT / "build/checks"
    output.mkdir(parents=True, exist_ok=True)
    results = {}
    with tempfile.TemporaryDirectory(prefix="semantic-protocol-", dir=output) as temp:
        root = Path(temp).resolve()
        cache = root / "cache"
        header = '#ifdef __cplusplus\nextern "C" {\n#endif\nint target(int);\nint alternate(int);\n#ifdef __cplusplus\n}\n#endif\n'
        a = '#include "api.h"\nint target(int x){return x+1;}\nint alternate(int x){return x-1;}\nint c_caller(int x){return target(x);}\n'
        b = '// 织源 UTF-8 offsets\n#include "api.h"\nint cpp_caller(){return target(7);}\n'
        (root / "api.h").write_text(header)
        (root / "a.c").write_text(a)
        (root / "b.cpp").write_text(b)
        commands = [{"directory": str(root), "file": file, "arguments": ["clang++" if file.endswith("cpp") else "clang", "-std=c++17" if file.endswith("cpp") else "-std=c11", "-I.", "-c", file, "-o", "out.o"]} for file in ["a.c", "b.cpp"]]
        (root / "compile_commands.json").write_text(json.dumps(commands))
        client = Client(root, cache)
        try:
            assert client.configure()["configured_units"] == 2
            client.rpc("semantic.index", {"paths": []})
            opened = client.rpc("document.open", {"path": "b.cpp"})
            results["initial"] = client.wait()
            disk_version = client.rpc("semantic.progress")["query_version"]
            assert disk_version["contract"] == "a2-query-v1"
            assert disk_version["workspace"] == str(root)
            assert set(disk_version["configuration"]) == {"id", "content_key", "provider"}
            assert disk_version["configuration"]["provider"].startswith("a2-clang-v3:")
            assert disk_version["disk"]["schema"] == "a2-store-v2"
            assert disk_version["disk"]["store_id"] and isinstance(disk_version["disk"]["revision"], int)
            assert disk_version["documents"] == []
            assert disk_version["coverage"] == {"kind": "persistent_index"}
            versioned_symbols = client.rpc("semantic.symbols", {"name": "target", "query_version": disk_version})
            assert versioned_symbols["query_version"] == disk_version
            target = next(s["id"] for s in versioned_symbols["rows"] if s["definition"])
            alternate = client.symbol("alternate")["id"]
            assert len(client.relations(target, "incoming")) == 2
            definitions = client.relations(target, "definitions")
            assert len(definitions) == 1 and definitions[0]["location"]["path"].endswith("a.c")
            offset = len(b[:b.index("target(7)")].encode())
            context = client.rpc("semantic.at", {"path": "b.cpp", "byte_offset": offset, "query_version": disk_version})
            assert context["entity_id"] == target and context["query_version"] == disk_version
            caller = context["enclosing_functions"][0]["id"]
            outgoing = client.rpc("semantic.relations", {"entity_id": caller, "direction": "outgoing", "query_version": disk_version})
            assert outgoing["rows"][0]["target"] == target and outgoing["query_version"] == disk_version
            print("PASS cross C/C++ linkage, definitions/references/calls and UTF-8 locations")

            mutations = [
                ("workspace", lambda v: v.update(workspace="/wrong-workspace")),
                ("configuration id", lambda v: v["configuration"].update(id="wrong")),
                ("configuration content", lambda v: v["configuration"].update(content_key="0" * 64)),
                ("provider", lambda v: v["configuration"].update(provider="wrong")),
                ("disk", lambda v: v["disk"].update(revision=v["disk"]["revision"] + 1)),
                ("documents", lambda v: v.update(documents=[{"path": "b.cpp", "revision": 1, "content_key": "0" * 64}])),
                ("coverage", lambda v: v.update(coverage={"kind": "overlay_translation_unit", "unit_id": "wrong"})),
            ]
            for label, mutate in mutations:
                candidate = copy.deepcopy(disk_version)
                mutate(candidate)
                error = client.rpc("semantic.relations", {"entity_id": target, "direction": "definitions", "limit": 1, "query_version": candidate}, expect_error=True)
                assert error["code"] == "stale_query_version", (label, error)
            legacy = copy.deepcopy(disk_version)
            legacy["contract"] = "a2-query-v0"
            error = client.rpc("semantic.relations", {"entity_id": target, "direction": "definitions", "query_version": legacy}, expect_error=True)
            assert error["code"] == "unsupported_query_version", error
            error = client.rpc("semantic.relations", {"entity_id": target, "direction": "definitions", "query_version": {"contract": "a2-query-v1"}}, expect_error=True)
            assert error["code"] == "invalid_params", error
            unknown = copy.deepcopy(disk_version)
            unknown["generation"] = 0
            error = client.rpc("semantic.relations", {"entity_id": target, "direction": "definitions", "query_version": unknown}, expect_error=True)
            assert error["code"] == "invalid_params", error
            print("PASS unified query version identity, full mismatch rejection and legacy compatibility")

            edited = client.rpc("document.edit", {"path": "b.cpp", "base_revision": opened["analysis"]["revision"], "start_byte": offset, "old_end_byte": offset + 6, "new_text": "alternate"})
            error = client.rpc("semantic.relations", {"entity_id": target, "direction": "incoming", "query_version": disk_version}, expect_error=True)
            assert error["code"] == "stale_query_version", error
            assert len(client.relations(target, "incoming")) == 1
            preview = client.rpc("semantic.preview", {"path": "b.cpp", "revision": edited["analysis"]["revision"]})
            assert preview["overlay"] and not preview["persisted"]
            assert preview["snapshot"]["calls"][0]["target"] == alternate, preview
            assert (root / "b.cpp").read_text() == b
            print("PASS unsaved overlay uses compiler semantics without publishing disk facts")

            b = b.replace("target(7)", "alternate(7)")
            (root / "b.cpp").write_text(b)
            client.rpc("document.open", {"path": "b.cpp"})
            results["single_file_update"] = client.index(["b.cpp"])
            assert results["single_file_update"]["total"] == 1
            assert len(client.relations(target, "incoming")) == 1
            assert len(client.relations(alternate, "incoming")) == 1
            print("PASS local saved-file replacement removes old call edges")

            (root / "api.h").write_text(header + "// header changed\n")
            results["header_update"] = client.index(["api.h"])
            assert results["header_update"]["total"] == 2
            assert results["header_update"]["analyzed"] == 2
            print("PASS header dependency selects both affected translation units")

            (root / "b.cpp").write_text('#include "missing-header.h"\n')
            results["compile_failure"] = client.index(["b.cpp"], success=False)
            assert results["compile_failure"]["state"] == "complete_with_errors"
            assert not client.relations(alternate, "incoming")
            assert len(client.relations(target, "definitions")) == 1
            (root / "b.cpp").write_text(b)
            client.index(["b.cpp"])
            print("PASS compiler errors suppress stale contributions and preserve other TUs")
            b += "// external saved change\n"
            (root / "b.cpp").write_text(b)
            error = client.rpc("semantic.at", {"path": "b.cpp", "byte_offset": offset}, expect_error=True)
            assert error["code"] == "stale_document"
            error = client.rpc("semantic.at", {"path": str(root / "b.cpp"), "byte_offset": offset}, expect_error=True)
            assert error["code"] == "stale_document"
            client.rpc("document.open", {"path": "b.cpp"})
            client.index(["b.cpp"])
            print("PASS positional queries reject facts for an outdated reader buffer")
        finally:
            client.close()

        client = Client(root, cache)
        try:
            client.configure()
            results["restart"] = client.index()
            assert results["restart"]["analyzed"] == 0 and results["restart"]["reused"] == 2
            print("PASS restart reuses semantic facts without starting AST workers")
            for command in commands:
                command["arguments"].append("-DSYMWEFT_VARIANT=1")
            (root / "compile_commands.json").write_text(json.dumps(commands))
            client.rpc("semantic.symbols", {"name": "target"}, expect_error=True)
            client.configure()
            assert client.rpc("semantic.symbols", {"name": "target"})["rows"] == []
            results["configuration_update"] = client.index()
            assert results["configuration_update"]["analyzed"] == 2
            print("PASS changed compile flags replace old compilation-unit ownership")

            (root / "b.cpp").write_text(b + "\n" + "\n".join("int generated_%d(){return alternate(%d);}" % (i, i) for i in range(2000)))
            start = time.monotonic()
            client.rpc("semantic.index", {"paths": ["b.cpp"]})
            client.rpc("semantic.cancel")
            results["cancel"] = client.wait(success=False)
            assert time.monotonic() - start < 3
            assert results["cancel"]["state"] in ["cancelled", "complete"]
            client.rpc("document.read", {"path": "a.c"})
            print("PASS cancellation completes promptly and source reading stays available")
        finally:
            client.close()
    (output / "semantic-protocol.json").write_text(json.dumps(results, indent=2))
    print("All semantic protocol checks passed.")


if __name__ == "__main__":
    main()
