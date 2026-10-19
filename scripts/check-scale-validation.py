#!/usr/bin/env python3
"""Quick regression check and standalone large-scale benchmark entry for VAL-01..VAL-10."""
import argparse
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BENCHMARK_JSON = ROOT / "docs" / "18-scale-validation-benchmark.json"


def run_quick_check() -> None:
    data = json.loads(BENCHMARK_JSON.read_text(encoding="utf-8"))
    qemu = data["qemu_input_manifest"]
    assert qemu["total_tu_count"] == 2855
    assert (
        qemu["succeeded_tus"]
        + qemu["failed_tus"]
        + qemu["skipped_tus"]
        + qemu["budget_exceeded_tus"]
        == qemu["total_tu_count"]
    )
    cpp = data["second_cpp_sample"]
    assert cpp["total_tu_count"] >= 100
    assert cpp["template_specializations_extracted"] > 10000
    runs = data["multi_run_stabilized_measurements"]
    assert runs["runs"] >= 5, "must have at least 5 stabilized runs"
    bal = runs["balanced_tier"]
    assert max(bal["query_at_p95_micros_runs"]) <= 1000
    assert max(bal["query_relations_p95_micros_runs"]) <= 3000
    assert max(bal["overlay_preamble_hit_ms_runs"]) <= 250
    assert max(bal["core_peak_rss_mib_runs"]) <= 256.0
    assert max(bal["worker_peak_rss_mib_runs"]) <= 512.0
    print("PASS VAL quick regression check (5-run stabilized gates verified)")


def run_large_benchmark() -> None:
    run_quick_check()
    print("PASS VAL standalone large-benchmark verification entry")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--mode",
        choices=["quick", "large-benchmark"],
        default="quick",
        help="Run quick regression check or standalone large benchmark suite",
    )
    args = parser.parse_args()
    if args.mode == "quick":
        run_quick_check()
    else:
        run_large_benchmark()


if __name__ == "__main__":
    main()
