#!/usr/bin/env python3
import hashlib
import json
import math
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "harness"))
import scoring

CALIB_FILE = os.path.join(ROOT, "harness", "calibration-seed1.json")
REF_AXES_FILE = os.path.join(ROOT, "harness", "reference_axes.json")

with open(CALIB_FILE) as f:
    CALIB = json.load(f)["tracks"]
with open(REF_AXES_FILE) as f:
    REF_AXES = json.load(f)

TRACK_INFO = {
    "a": {
        "name": "Track A (Ingest)",
        "data": "/tmp/sd-full-a",
        "out_file": "summary.txt",
        "expected_hash": "495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3",
    },
    "b": {
        "name": "Track B (Index)",
        "data": "/tmp/sd-full-b",
        "out_file": "results.txt",
        "expected_hash": "f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927",
    },
    "c": {
        "name": "Track C (Join)",
        "data": "/tmp/sd-full-c",
        "out_file": "aggregates.txt",
        "expected_hash": "d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536",
    }
}

def count_track_lines(track_dir):
    # Formats src/main.rs with rustfmt --edition 2021 (default settings), counts with scc (Rust + Dockerfile)
    main_rs = os.path.join(track_dir, "src", "main.rs")
    if os.path.exists(main_rs):
        subprocess.run(["rustfmt", "--edition", "2021", main_rs], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    p = subprocess.run(["scc", "--format", "json", track_dir], capture_output=True, text=True, check=True)
    data = json.loads(p.stdout)
    # Sum code lines for Rust and Dockerfile (ignore Markdown, TOML, etc.)
    code = 0
    for item in data:
        lang = item.get("Name")
        if lang in ["Rust", "Dockerfile"]:
            code += item.get("Code", 0)
    return code

def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1024 * 1024):
            h.update(chunk)
    return h.hexdigest()

def benchmark_track(track_key, track_dir, runs=7):
    info = TRACK_INFO[track_key]
    bin_path = os.path.join(track_dir, "target", "release", "solution")

    print(f"Building {track_dir}...")
    env_build = os.environ.copy()
    env_build["RUSTFLAGS"] = "-C target-cpu=x86-64-v3 -C panic=abort"
    subprocess.run(["cargo", "build", "--release"], cwd=track_dir, env=env_build, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    lines = count_track_lines(track_dir)

    out_dir = os.path.join("/tmp", f"sd-calib-{track_key}")
    os.makedirs(out_dir, exist_ok=True)
    out_path = os.path.join(out_dir, info["out_file"])

    # Warmup disk cache
    for root, _, files in os.walk(info["data"]):
        for fname in files:
            try:
                with open(os.path.join(root, fname), "rb") as f:
                    while f.read(1024 * 1024): pass
            except Exception: pass

    cmd = ["/usr/bin/time", "-v", "taskset", "-c", "0-11", bin_path]
    env_run = os.environ.copy()
    env_run["DATA_DIR"] = info["data"]
    env_run["OUT_DIR"] = out_dir

    times = []
    rss_list = []

    for r in range(runs):
        if os.path.exists(out_path):
            os.remove(out_path)
        t_start = time.perf_counter()
        p = subprocess.run(cmd, env=env_run, capture_output=True, text=True)
        t_elapsed = time.perf_counter() - t_start
        if p.returncode != 0:
            print(f"FAILED run {r}: {p.stderr}")
            return None
        max_rss_kb = 0
        for line in p.stderr.splitlines():
            if "Maximum resident set size" in line:
                max_rss_kb = int(line.split(":")[-1].strip())
        times.append(t_elapsed)
        rss_list.append(max_rss_kb / 1024.0)

    # Median of middle 5 runs
    sorted_indices = sorted(range(len(times)), key=lambda i: times[i])
    mid_indices = sorted_indices[1:-1]
    mid_times = sorted([times[i] for i in mid_indices])
    mid_rss = sorted([rss_list[i] for i in mid_indices])
    T_s = mid_times[len(mid_times) // 2]
    M_mb = mid_rss[len(mid_rss) // 2]

    actual_hash = sha256_file(out_path)
    hash_ok = (actual_hash == info["expected_hash"])

    # Score calculation using calibration constants
    t_ref = CALIB[track_key]["T_ref_s"]
    m_ref_mb = CALIB[track_key]["M_ref_bytes"] / 1e6
    s_ref = REF_AXES[track_key]["S_lines"]

    ratios = {
        "T": t_ref / T_s,
        "M": m_ref_mb / M_mb,
        "S": s_ref / lines,
        "D": 1.0
    }
    K = scoring.k_from_ratios(ratios)
    pts = scoring.speed_points(K)

    return {
        "track": track_key.upper(),
        "hash_ok": hash_ok,
        "T_s": T_s,
        "T_ms": T_s * 1000.0,
        "M_mb": M_mb,
        "S": lines,
        "K": K,
        "pts": pts
    }

def main():
    benchmarks = [
        ("A", "baseline (12007d9)", "a", "/home/maddy/.gemini/antigravity-ide/scratch/wt-base-ab/track-a"),
        ("A", "current (HEAD)",    "a", "/home/maddy/.gemini/antigravity-ide/scratch/speed-demon-prep/track-a"),
        ("B", "baseline (12007d9)", "b", "/home/maddy/.gemini/antigravity-ide/scratch/wt-base-ab/track-b"),
        ("B", "current (HEAD)",    "b", "/home/maddy/.gemini/antigravity-ide/scratch/speed-demon-prep/track-b"),
        ("C", "baseline (1c52882)", "c", "/home/maddy/.gemini/antigravity-ide/scratch/wt-base-c/track-c"),
        ("C", "current (HEAD)",    "c", "/home/maddy/.gemini/antigravity-ide/scratch/speed-demon-prep/track-c"),
    ]

    results = []
    for track_name, commit_label, track_key, track_dir in benchmarks:
        print(f"\n==================================================")
        print(f"Benchmarking Track {track_name} [{commit_label}] (7 runs)...")
        print(f"==================================================")
        res = benchmark_track(track_key, track_dir, runs=7)
        if res:
            res["commit"] = commit_label
            results.append(res)
            print(f"--> T: {res['T_ms']:.2f} ms | M: {res['M_mb']:.2f} MB | S: {res['S']} | K: {res['K']:.3f} | Local Pts: {res['pts']:.2f} | Hash: {'PASS' if res['hash_ok'] else 'FAIL'}")

    print("\n" + "=" * 90)
    print("PHASE 0 CALIBRATION BENCHMARK RESULTS (7 runs, median of middle 5)")
    print("=" * 90)
    print(f"| Track | Commit | Hash OK | T (ms) | M (MB) | S (lines) | K | Local Pts |")
    print(f"| :--- | :--- | :---: | :---: | :---: | :---: | :---: | :---: |")
    for r in results:
        h = "PASS" if r["hash_ok"] else "FAIL"
        print(f"| Track {r['track']} | {r['commit']} | {h} | {r['T_ms']:.2f} ms | {r['M_mb']:.2f} MB | {r['S']} | {r['K']:.3f} | {r['pts']:.2f} |")

if __name__ == "__main__":
    main()
