"""Opt-in macOS process-tree sampling; only caller-owned roots/descendants.

Uses the installed SDK's rusage_info_v2 ABI. CPU counters exclude child CPU.
No ps subprocess per sample, command arguments, or machine-wide process dump.
"""
import collections
import ctypes
import json
import resource
import sys
import threading
import time
from pathlib import Path


class RUsageV2(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(name, ctypes.c_uint64) for name in [
        "user", "system", "idle_wakeups", "interrupt_wakeups", "pageins", "wired",
        "rss", "footprint", "start", "exit", "child_user", "child_system",
        "child_idle_wakeups", "child_interrupt_wakeups", "child_pageins",
        "child_elapsed", "disk_read", "disk_written"]]


class MacProcesses:
    def __init__(self):
        if sys.platform != "darwin":
            raise RuntimeError("Native resource probe currently verified on macOS only")
        assert ctypes.sizeof(RUsageV2) == 160
        self.lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        self.lib.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
        self.lib.proc_pid_rusage.restype = ctypes.c_int
        self.lib.proc_listchildpids.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
        self.lib.proc_listchildpids.restype = ctypes.c_int
        self.lib.proc_name.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
        self.lib.proc_name.restype = ctypes.c_int
        class Timebase(ctypes.Structure):
            _fields_ = [("numer",ctypes.c_uint32),("denom",ctypes.c_uint32)]
        system = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
        system.mach_timebase_info.argtypes = [ctypes.POINTER(Timebase)]
        system.mach_timebase_info.restype = ctypes.c_int
        timebase = Timebase()
        if system.mach_timebase_info(ctypes.byref(timebase)) != 0 or not timebase.denom:
            raise RuntimeError("mach timebase unavailable")
        # rusage_info CPU fields use Mach absolute time, not nanoseconds on M2.
        self.cpu_tick_seconds = timebase.numer / timebase.denom / 1e9

    def read(self, pid):
        info = RUsageV2()
        if self.lib.proc_pid_rusage(pid, 2, ctypes.byref(info)) != 0:
            return None
        name = ctypes.create_string_buffer(256)
        self.lib.proc_name(pid, name, len(name))
        return dict(pid=pid, start_id=str(info.start), name=name.value.decode(errors="replace"),
                    cpu_seconds=(info.user + info.system) * self.cpu_tick_seconds, rss_bytes=info.rss,
                    physical_footprint_bytes=info.footprint,
                    disk_read_bytes=info.disk_read, disk_written_bytes=info.disk_written)

    def children(self, pid):
        # This API returns a PID count, unlike proc_listpids' byte count.
        buffer = (ctypes.c_int * 512)()
        count = self.lib.proc_listchildpids(pid, buffer, ctypes.sizeof(buffer))
        return list(buffer[:max(0, min(count, len(buffer)))]), count >= len(buffer)


def summarize(samples, phase=None, unix_bounds=None):
    selected = [s for s in samples if (phase is None or s["phase"] == phase)
                and (unix_bounds is None or unix_bounds[0] <= s["unix_ns"] <= unix_bounds[1])]
    result = dict(samples=len(selected), tree_rss_sampled_max_bytes=0,
                  tree_footprint_sampled_max_bytes=0, roles={}, cpu_observed_seconds=0,
                  cpu_interval_max_one_core_percent=0, incomplete_cpu_intervals=0,
                  collection_max_ms=0, truncated=False)
    previous = None
    for sample in selected:
        role_totals = collections.defaultdict(lambda: [0, 0])
        cpu = 0
        complete = sample["cpu_complete"]
        # A boundary interval must not borrow CPU from outside the selected phase.
        interval_valid = previous is not None and sample["previous_monotonic_ns"] == previous["monotonic_ns"]
        for row in sample["processes"]:
            role = row["role"]
            metrics = result["roles"].setdefault(role, dict(pids=[], rss_sampled_max_bytes=0,
                footprint_sampled_max_bytes=0, cpu_observed_seconds=0))
            identity = f'{row["pid"]}:{row["start_id"]}'
            if identity not in metrics["pids"]:
                metrics["pids"].append(identity)
            role_totals[role][0] += row["rss_bytes"]
            role_totals[role][1] += row["physical_footprint_bytes"]
            if interval_valid and row.get("cpu_delta_seconds") is not None:
                metrics["cpu_observed_seconds"] += row["cpu_delta_seconds"]
                cpu += row["cpu_delta_seconds"]
        result["tree_rss_sampled_max_bytes"] = max(result["tree_rss_sampled_max_bytes"], sum(r[0] for r in role_totals.values()))
        result["tree_footprint_sampled_max_bytes"] = max(result["tree_footprint_sampled_max_bytes"], sum(r[1] for r in role_totals.values()))
        for role, (rss, footprint) in role_totals.items():
            result["roles"][role]["rss_sampled_max_bytes"] = max(result["roles"][role]["rss_sampled_max_bytes"], rss)
            result["roles"][role]["footprint_sampled_max_bytes"] = max(result["roles"][role]["footprint_sampled_max_bytes"], footprint)
        result["cpu_observed_seconds"] += cpu
        if interval_valid and sample["interval_seconds"] > 0:
            result["cpu_interval_max_one_core_percent"] = max(result["cpu_interval_max_one_core_percent"], 100 * cpu / sample["interval_seconds"])
        result["incomplete_cpu_intervals"] += int(not complete or not interval_valid)
        result["collection_max_ms"] = max(result["collection_max_ms"], sample["collection_micros"] / 1000)
        result["truncated"] |= sample["truncated"]
        previous = sample
    if len(selected) > 1:
        span = (selected[-1]["monotonic_ns"] - selected[0]["monotonic_ns"]) / 1e9
        result["observed_span_seconds"] = span
        result["cpu_average_one_core_percent"] = 100 * result["cpu_observed_seconds"] / span if span else None
        intervals = [(b["monotonic_ns"] - a["monotonic_ns"]) / 1e6 for a, b in zip(selected, selected[1:])]
        result["interval_ms"] = dict(mean=sum(intervals) / len(intervals), max=max(intervals))
    result["available"] = any(s["processes"] for s in selected)
    if not result["available"]:
        result["tree_rss_sampled_max_bytes"] = None
        result["tree_footprint_sampled_max_bytes"] = None
        result["cpu_observed_seconds"] = None
        result["cpu_interval_max_one_core_percent"] = None
    return result


class ResourceSampler:
    def __init__(self, roots, output, interval_ms=50):
        if not 25 <= interval_ms <= 1000:
            raise ValueError("sample interval must be 25..1000 ms")
        self.backend = MacProcesses()
        self.roots = dict(roots)
        self.root_identity = {}
        self.interval = interval_ms / 1000
        self.output = Path(output)
        self.output.parent.mkdir(parents=True, exist_ok=True)
        self.samples = collections.deque(maxlen=20000)
        self.phase = "setup"
        self.previous = {}
        self.previous_time = None
        self.previous_phase = None
        self.stop_event = threading.Event()
        self.thread = None
        self.thread_cpu_seconds = 0
        self.error = None
        self.overwritten_samples = 0

    def set_phase(self, name):
        self.phase = name

    def sample(self):
        start = time.monotonic_ns()
        unix_ns = time.time_ns()
        phase = self.phase
        rows, current, visited = [], {}, set()
        queue = [(pid, None, role) for pid, role in self.roots.items()]
        truncated = False
        while queue and len(visited) < 128:
            pid, parent, role = queue.pop(0)
            if pid in visited:
                continue
            visited.add(pid)
            row = self.backend.read(pid)
            if row is None:
                continue
            if pid in self.roots:
                previous_identity = self.root_identity.setdefault(pid, row["start_id"])
                if previous_identity != row["start_id"]:
                    continue  # Never follow a reused root PID into an unrelated app.
            if role is None:
                role = "core" if parent == "ui" and row["name"] == "symweft-core" else "compiler_worker" if parent == "core" and row["name"] == "symweft-core" else "helper"
            row["role"] = role
            key = (pid, row["start_id"])
            previous_cpu = self.previous.get(key)
            row["cpu_delta_seconds"] = max(0, row["cpu_seconds"] - previous_cpu) if previous_cpu is not None and phase == self.previous_phase else None
            current[key] = row["cpu_seconds"]
            rows.append(row)
            children, overflow = self.backend.children(pid)
            truncated |= overflow
            queue.extend((child, role, None) for child in children if child > 0)
        truncated |= bool(queue)
        complete = current.keys() == self.previous.keys() and phase == self.previous_phase
        sample = dict(monotonic_ns=start, unix_ns=unix_ns, phase=phase, processes=rows,
            previous_monotonic_ns=self.previous_time, interval_seconds=(start - self.previous_time) / 1e9 if self.previous_time else 0,
            cpu_complete=complete, truncated=truncated, collection_micros=(time.monotonic_ns() - start) / 1000)
        self.previous, self.previous_time, self.previous_phase = current, start, phase
        return sample

    def _run(self):
        cpu = time.thread_time_ns()
        try:
            with self.output.open("w") as output:
                while not self.stop_event.is_set():
                    started = time.monotonic()
                    sample = self.sample()
                    if len(self.samples) == self.samples.maxlen:
                        self.overwritten_samples += 1
                    self.samples.append(sample)
                    output.write(json.dumps(sample, separators=(",", ":")) + "\n")
                    output.flush()
                    self.stop_event.wait(max(0, self.interval - (time.monotonic() - started)))
        except Exception as error:
            self.error = str(error)
        finally:
            self.thread_cpu_seconds = (time.thread_time_ns() - cpu) / 1e9

    def start(self):
        self.thread = threading.Thread(target=self._run, name="symweft-resource-sampler", daemon=True)
        self.thread.start()
        return self

    def stop(self):
        self.stop_event.set()
        if self.thread:
            self.thread.join(timeout=3)
            if self.thread.is_alive():
                raise RuntimeError("resource sampler failed to stop")
        if self.error:
            raise RuntimeError(self.error)
        return dict(backend="libproc proc_pid_rusage v2 / proc_listchildpids", interval_requested_ms=self.interval * 1000,
                    cpu_tick_seconds=self.backend.cpu_tick_seconds,
                    sampling_thread_cpu_seconds=self.thread_cpu_seconds, overwritten_samples=self.overwritten_samples,
                    harness_peak_rss_bytes=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
                    summary=summarize(list(self.samples)))
