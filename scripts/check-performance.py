#!/usr/bin/env python3
"""Validate native units, concurrent aggregation and opt-in production profiles."""
import importlib.util
import json
import os
from pathlib import Path
import resource
import subprocess
import sys
import tempfile
import time
import unittest

from resource_probe import MacProcesses, ResourceSampler, summarize

PROJECT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("protocol", PROJECT / "scripts/check-semantic.py")
protocol = importlib.util.module_from_spec(spec)
spec.loader.exec_module(protocol)


class Accounting(unittest.TestCase):
    def test_tree_peak_uses_same_sample_and_cpu_boundaries(self):
        def sample(t, rss):
            return dict(phase="test", monotonic_ns=t * 1000000000, previous_monotonic_ns=(t-1)*1000000000 if t else None,
                unix_ns=t, cpu_complete=True, interval_seconds=1, collection_micros=1, truncated=False,
                processes=[dict(pid=n+1, start_id="1", role="core", rss_bytes=r,
                    physical_footprint_bytes=r, cpu_delta_seconds=.1 if t else None) for n,r in enumerate(rss)])
        result = summarize([sample(0,[100,20]),sample(1,[20,100])])
        self.assertEqual(result["tree_rss_sampled_max_bytes"],120)  # Not 100+100.
        self.assertAlmostEqual(result["cpu_observed_seconds"],.2)
        clipped = summarize([sample(0,[100,20]),sample(1,[20,100])],unix_bounds=(1,1))
        self.assertEqual(clipped["cpu_observed_seconds"],0)  # No CPU outside phase.

    @unittest.skipUnless(sys.platform=="darwin","macOS ABI check")
    def test_native_cpu_units_match_getrusage(self):
        probe = MacProcesses()
        before = probe.read(os.getpid())
        clock = time.process_time()
        begin = resource.getrusage(resource.RUSAGE_SELF)
        while time.process_time()-clock < .06:
            sum(i*i for i in range(1000))
        end = resource.getrusage(resource.RUSAGE_SELF)
        after = probe.read(os.getpid())
        expected = end.ru_utime+end.ru_stime-begin.ru_utime-begin.ru_stime
        actual = after["cpu_seconds"]-before["cpu_seconds"]
        self.assertLess(abs(actual-expected),.01)
        self.assertGreater(after["rss_bytes"],1024*1024)
        self.assertEqual(before["start_id"],after["start_id"])

    @unittest.skipUnless(sys.platform=="darwin","macOS process tree check")
    def test_owned_descendants_are_sampled_and_reaped(self):
        child = "import time; data=bytearray(24*1024*1024); print('ready',flush=True); time.sleep(.5)"
        parent = "import subprocess,sys; p=subprocess.Popen([sys.executable,'-c',"+repr(child)+"]); p.wait()"
        process = subprocess.Popen([sys.executable,"-c",parent],stdout=subprocess.PIPE,text=True)
        with tempfile.TemporaryDirectory(prefix="symweft-probe-") as folder:
            sampler = ResourceSampler([(process.pid,"core")],Path(folder)/"trace.jsonl",25).start()
            try:
                self.assertEqual(process.stdout.readline().strip(),"ready")
                time.sleep(.12)
                process.wait(timeout=3)
            finally:
                if process.poll() is None: process.kill();process.wait()
                summary = sampler.stop()["summary"]
                process.stdout.close()
            self.assertIn("helper",summary["roles"])
            self.assertGreater(summary["tree_rss_sampled_max_bytes"],24*1024*1024)
            self.assertFalse(summary["truncated"])

    def test_opt_in_profiles_preserve_semantics_and_measure_real_worker(self):
        with tempfile.TemporaryDirectory(prefix="symweft-profile-") as folder:
            root = Path(folder).resolve()
            (root/"api.h").write_text("int target(int);\n")
            (root/"a.c").write_text('#include "api.h"\nint target(int x){return x;} int a(){return target(1);}\n')
            (root/"b.c").write_text('#include "api.h"\nint b(){return target(2);}\n')
            commands=[dict(directory=str(root),file=p,arguments=["clang","-c",p]) for p in ["a.c","b.c"]]
            (root/"compile_commands.json").write_text(json.dumps(commands))
            answers=[]
            old=os.environ.get("SYMWEFT_PROFILE")
            try:
                for enabled in [False,True]:
                    os.environ["SYMWEFT_PROFILE"]="1" if enabled else "0"
                    client=protocol.Client(root,root/("on" if enabled else "off"))
                    try:
                        client.configure()
                        job=client.index()
                        target=client.symbol("target")["id"]
                        answers.append(client.relations(target,"incoming"))
                        if not enabled:
                            self.assertNotIn("profile",job)
                            continue
                        units=job["profile"]["units"]
                        self.assertEqual(len(units),2)
                        worker_pids=set()
                        for unit in units:
                            run=unit["run"];worker=run["worker"];store=unit["publish"]
                            worker_pids.add(worker["pid"])
                            self.assertNotEqual(worker["pid"],client.process.pid)
                            self.assertEqual(worker["reply_bytes"],run["reply_bytes"])
                            self.assertGreater(worker["parse_micros"],0)
                            self.assertGreater(worker["fact_extract_micros"],0)
                            self.assertGreater(worker["self_peak_rss_bytes"],0)
                            self.assertGreaterEqual(worker["worker_wall_micros"],worker["provider_wall_micros"])
                            self.assertGreaterEqual(run["controller_wall_micros"],run["reply_decode_micros"]+run["child_wait_micros"])
                            self.assertGreater(store["fact_encode_nanos"],0)
                            self.assertGreater(store["publish_wall_micros"],0)
                        self.assertEqual(len(worker_pids),2)
                        self.assertGreater(units[1]["publish"]["reused_facts"],0)
                        reused=client.index()
                        self.assertEqual(reused["reused"],2)
                        self.assertTrue(all(u["reused"] and "run" not in u for u in reused["profile"]["units"]))
                        (root/"b.c").write_text("int broken( { }\n")
                        failed=client.index(["b.c"],success=False)
                        self.assertEqual(failed["state"],"complete_with_errors")
                        self.assertGreater(failed["profile"]["units"][0]["run"]["worker"]["parse_micros"],0)
                    finally:client.close()
            finally:
                if old is None:os.environ.pop("SYMWEFT_PROFILE",None)
                else:os.environ["SYMWEFT_PROFILE"]=old
            self.assertEqual(answers[0],answers[1])


if __name__=="__main__":
    unittest.main(verbosity=2)
