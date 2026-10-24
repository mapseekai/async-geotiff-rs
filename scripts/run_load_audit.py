"""Measure an already-built load_audit binary (macOS/Linux, Python stdlib only).

Build separately so compiler memory does not contaminate the workload's peak RSS:
  cargo build --release --locked --example load_audit --features mosaic
  python3 scripts/run_load_audit.py --output target/load-audit/results.json
"""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import time
import zlib


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/examples/load_audit"))
    parser.add_argument("--profile", default="release")
    parser.add_argument("--samples", type=int, default=32)
    parser.add_argument("--concurrency", type=int, default=4)
    parser.add_argument("--output", type=Path, default=Path("target/load-audit/results.json"))
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    os.chdir(root)
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f"build the workload binary first: {binary}")
    if not 4 <= args.samples <= 128 or not 1 <= args.concurrency <= 16:
        parser.error("samples must be 4..128 and concurrency 1..16")
    command = [str(binary), str(args.samples), str(args.concurrency)]
    started = time.monotonic()
    with tempfile.TemporaryFile(mode="w+") as stdout, tempfile.TemporaryFile(mode="w+") as stderr:
        process = subprocess.Popen(command, stdout=stdout, stderr=stderr)
        while True:
            pid, status, usage = os.wait4(process.pid, os.WNOHANG)
            if pid:
                process.returncode = os.waitstatus_to_exitcode(status)
                break
            if time.monotonic() - started > 120:
                process.kill()
                _, status, usage = os.wait4(process.pid, 0)
                process.returncode = os.waitstatus_to_exitcode(status)
                raise SystemExit("workload exceeded the 120 second safety limit")
            time.sleep(0.05)
        stdout.seek(0)
        stderr.seek(0)
        output, errors = stdout.read(), stderr.read()
    if process.returncode:
        raise SystemExit(f"workload failed ({process.returncode}):\n{errors}\n{output}")
    elapsed = time.monotonic() - started
    peak_rss = usage.ru_maxrss if platform.system() == "Darwin" else usage.ru_maxrss * 1024
    def capture(*cmd):
        return subprocess.check_output(cmd, text=True).strip()
    report = {
        "measured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": platform.platform(), "machine": platform.machine(),
        "logical_cpus": os.cpu_count(), "rustc": capture("rustc", "-Vv"),
        "git_head": capture("git", "rev-parse", "HEAD"),
        "git_status_porcelain": capture("git", "status", "--porcelain"),
        "binary": str(binary), "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "profile_label": args.profile, "features": "default + mosaic", "command": command,
        "fixture_sha256": hashlib.sha256((root / "tests/fixtures/rgb_deflate.tif").read_bytes()).hexdigest(),
        "zlib_runtime": zlib.ZLIB_RUNTIME_VERSION,
        "peak_rss_bytes": peak_rss, "rss_method": "wait4 child ru_maxrss (Darwin bytes; Linux KiB converted)",
        "process_wall_seconds": elapsed,
        "process_user_seconds": usage.ru_utime, "process_system_seconds": usage.ru_stime,
        "workload": json.loads(output), "stderr": errors,
        "limits": "Synthetic 64x64 Deflate source; local loopback with injected delay; one bounded run, not production capacity or a memory proof.",
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"Saved {args.output}; peak RSS {peak_rss / 1024**2:.2f} MiB")
    for result in report["workload"]["results"]:
        print(result["scenario"], result.get("latency_ms", "bounded-resource observation"))


if __name__ == "__main__":
    main()
