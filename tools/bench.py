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

# Calibrated ratios: r_axis = axis_board / axis_local_baseline
BOARD_RATIOS = {
    "a": {"r_T": 0.621028, "r_M": 1.102107, "r_S": 1.0},
    "b": {"r_T": 1.043278, "r_M": 1.051816, "r_S": 1.0},
    "c": {"r_T": 1.030774, "r_M": 1.095224, "r_S": 1.0},
}

TRACKS = {
    "a": {
        "name": "Track A (Ingest)",
        "dir": "track-a",
        "data": "/tmp/sd-full-a",
        "out_file": "summary.txt",
        "expected_hash": "495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3",
        "tiers": [(80.3, "T1 (80.3)"), (85.5, "T2 (85.5)"), (89.3, "T3 (89.3)")]
    },
    "b": {
        "name": "Track B (Index)",
        "dir": "track-b",
        "data": "/tmp/sd-full-b",
        "out_file": "results.txt",
        "expected_hash": "f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927",
        "tiers": [(63.6, "T1 (63.6)"), (70.0, "T2 (70.0)"), (75.8, "T3 (75.8)")]
    },
    "c": {
        "name": "Track C (Join)",
        "dir": "track-c",
        "data": "/tmp/sd-full-c",
        "out_file": "aggregates.txt",
        "expected_hash": "d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536",
        "tiers": [(74.9, "T1 (74.9)"), (79.9, "T2 (79.9)"), (84.2, "T3 (84.2)")]
    }
}

def count_track_lines(track_dir):
    main_rs = os.path.join(track_dir, "src", "main.rs")
    if os.path.exists(main_rs):
        subprocess.run(["rustfmt", "--edition", "2021", main_rs], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    p = subprocess.run(["scc", "--format", "json", track_dir], capture_output=True, text=True, check=True)
    data = json.loads(p.stdout)
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

def score_track(key, T_s, M_mb, S_lines, deps=0):
    t_ref = CALIB[key]["T_ref_s"]
    m_ref_mb = CALIB[key]["M_ref_bytes"] / 1e6
    s_ref = REF_AXES[key]["S_lines"]
    ratios = {
        "T": t_ref / T_s,
        "M": m_ref_mb / M_mb,
        "S": s_ref / S_lines,
        "D": 1.0 / (1.0 + deps)
    }
    K = scoring.k_from_ratios(ratios)
    pts = scoring.speed_points(K)
    return K, pts

def get_cgroup_peak(cg_name, cmd, env):
    cg = f"/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/{cg_name}"
    if os.path.exists(cg):
        try: os.rmdir(cg)
        except Exception: pass
    os.makedirs(cg, exist_ok=True)

    def enter_cg():
        with open(f"{cg}/cgroup.procs", "w") as f:
            f.write(f"{os.getpid()}\n")

    t_start = time.perf_counter()
    p = subprocess.run(cmd, env=env, preexec_fn=enter_cg, capture_output=True, text=True)
    t_elapsed = time.perf_counter() - t_start

    peak_bytes = 0
    try:
        with open(f"{cg}/memory.peak", "r") as f:
            peak_bytes = int(f.read().strip())
    except Exception: pass

    try: os.rmdir(cg)
    except Exception: pass

    max_rss_kb = 0
    for line in p.stderr.splitlines():
        if "Maximum resident set size" in line:
            max_rss_kb = int(line.split(":")[-1].strip())

    return p, t_elapsed, peak_bytes / 1e6, max_rss_kb / 1024.0

def evaluate_track(key, runs=7):
    info = TRACKS[key]
    track_dir = os.path.join(ROOT, info["dir"])
    bin_path = os.path.join(track_dir, "target", "release", "solution")

    print(f"\n--- Benchmarking {info['name']} ({runs} runs) ---")
    build_cmd = ["cargo", "build", "--release"]
    env_build = os.environ.copy()
    env_build["RUSTFLAGS"] = "-C target-cpu=x86-64-v3 -C panic=abort"
    subprocess.run(build_cmd, cwd=track_dir, env=env_build, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    code_lines = count_track_lines(track_dir)

    out_dir = os.path.join("/tmp", f"sd-eval-{key}")
    os.makedirs(out_dir, exist_ok=True)
    out_path = os.path.join(out_dir, info["out_file"])

    # Warm up disk cache for input data
    for root, _, files in os.walk(info["data"]):
        for fname in files:
            try:
                with open(os.path.join(root, fname), "rb") as f:
                    while f.read(1024 * 1024): pass
            except Exception: pass

    cmd = ["/usr/bin/time", "-v", "taskset", "-c", "0-7", bin_path]
    env = os.environ.copy()
    env["DATA_DIR"] = info["data"]
    env["OUT_DIR"] = out_dir

    times = []
    peaks = []
    hwm_list = []

    for r in range(runs):
        if os.path.exists(out_path):
            os.remove(out_path)
        p, t_elapsed, peak_mb, hwm_mb = get_cgroup_peak(f"sd-bench-{key}-{r}", cmd, env)
        if p.returncode != 0:
            print(f"FAILED with code {p.returncode}: {p.stderr}")
            return None
        times.append(t_elapsed)
        peaks.append(peak_mb)
        hwm_list.append(hwm_mb)

    # Median of middle 5 runs
    if len(times) >= 5:
        sorted_indices = sorted(range(len(times)), key=lambda i: times[i])
        mid_indices = sorted_indices[1:-1]
        mid_times = sorted([times[i] for i in mid_indices])
        mid_peaks = sorted([peaks[i] for i in mid_indices])
        mid_hwm = sorted([hwm_list[i] for i in mid_indices])
        T_med = mid_times[len(mid_times) // 2]
        M_med = mid_peaks[len(mid_peaks) // 2]
        HWM_med = mid_hwm[len(mid_hwm) // 2]
    else:
        T_med = sorted(times)[len(times) // 2]
        M_med = sorted(peaks)[len(peaks) // 2]
        HWM_med = sorted(hwm_list)[len(hwm_list) // 2]

    actual_hash = sha256_file(out_path)
    hash_ok = (actual_hash == info["expected_hash"])

    K_local, pts_local = score_track(key, T_med, M_med, code_lines, deps=0)

    # Board estimate using calibrated ratios
    r_t = BOARD_RATIOS[key]["r_T"]
    r_m = BOARD_RATIOS[key]["r_M"]
    r_s = BOARD_RATIOS[key]["r_S"]
    K_board, pts_board_est = score_track(key, T_med * r_t, M_med * r_m, code_lines * r_s, deps=0)

    tier_status = "Below T1"
    for threshold, name in info["tiers"]:
        if pts_board_est >= threshold:
            tier_status = name

    print(f"Hash Verified: {'PASS' if hash_ok else 'FAIL (MISMATCH!)'}")
    print(f"Time (T): {T_med*1000:.2f} ms ({T_med:.4f} s) [Board Est: {T_med*r_t*1000:.2f} ms]")
    print(f"RAM Peak (M): {M_med:.2f} MB (cgroup memory.peak), VmHWM: {HWM_med:.2f} MB [Board Est: {M_med*r_m:.2f} MB]")
    print(f"Code Lines (S): {code_lines} (rustfmt + scc)")
    print(f"Local Score: {pts_local:.2f} pts (K={K_local:.3f})")
    print(f"Board Estimate: {pts_board_est:.2f} pts (K={K_board:.3f}) [{tier_status}]")

    return {
        "key": key,
        "name": info["name"],
        "hash_ok": hash_ok,
        "T_s": T_med,
        "M_mb": M_med,
        "HWM_mb": HWM_med,
        "S": code_lines,
        "K": K_local,
        "pts_local": pts_local,
        "pts_board_est": pts_board_est,
        "tier": tier_status
    }

def run_paired_comparison(key, cand_dir, base_dir, pairs=15, warmups=2):
    info = TRACKS[key]
    cand_bin = os.path.join(cand_dir, "target", "release", "solution")
    base_bin = os.path.join(base_dir, "target", "release", "solution")

    print(f"\n=======================================================")
    print(f"PAIRED BENCHMARK PROTOCOL (15 pairs alternating B/C)")
    print(f"Track: {info['name']}")
    print(f"Candidate: {cand_dir}")
    print(f"Baseline:  {base_dir}")
    print(f"=======================================================")

    env_build = os.environ.copy()
    env_build["RUSTFLAGS"] = "-C target-cpu=x86-64-v3 -C panic=abort"
    subprocess.run(["cargo", "build", "--release"], cwd=cand_dir, env=env_build, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(["cargo", "build", "--release"], cwd=base_dir, env=env_build, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    cand_lines = count_track_lines(cand_dir)
    base_lines = count_track_lines(base_dir)

    out_dir_cand = os.path.join("/tmp", f"sd-paired-cand-{key}")
    out_dir_base = os.path.join("/tmp", f"sd-paired-base-{key}")
    os.makedirs(out_dir_cand, exist_ok=True)
    os.makedirs(out_dir_base, exist_ok=True)
    out_path_cand = os.path.join(out_dir_cand, info["out_file"])
    out_path_base = os.path.join(out_dir_base, info["out_file"])

    env_cand = os.environ.copy()
    env_cand["DATA_DIR"] = info["data"]
    env_cand["OUT_DIR"] = out_dir_cand

    env_base = os.environ.copy()
    env_base["DATA_DIR"] = info["data"]
    env_base["OUT_DIR"] = out_dir_base

    cmd_cand = ["/usr/bin/time", "-v", "taskset", "-c", "0-7", cand_bin]
    cmd_base = ["/usr/bin/time", "-v", "taskset", "-c", "0-7", base_bin]

    print(f"Running {warmups} warmup rounds...")
    for w in range(warmups):
        get_cgroup_peak(f"sd-paired-w-{key}", cmd_base, env_base)
        get_cgroup_peak(f"sd-paired-w-{key}", cmd_cand, env_cand)

    cand_times = []
    base_times = []
    cand_peaks = []
    base_peaks = []
    pair_ratios = []
    cand_wins = 0
    all_hashes_ok = True

    print(f"Running {pairs} alternating pairs (B, C, B, C...)...")
    for i in range(pairs):
        # Run Baseline (B)
        if os.path.exists(out_path_base): os.remove(out_path_base)
        p_b, t_b, peak_b, hwm_b = get_cgroup_peak(f"sd-paired-b-{i}", cmd_base, env_base)
        h_b = sha256_file(out_path_base)
        if h_b != info["expected_hash"]:
            all_hashes_ok = False
            print(f"Pair {i+1}: BASELINE HASH MISMATCH! ({h_b})")

        # Run Candidate (C)
        if os.path.exists(out_path_cand): os.remove(out_path_cand)
        p_c, t_c, peak_c, hwm_c = get_cgroup_peak(f"sd-paired-c-{i}", cmd_cand, env_cand)
        h_c = sha256_file(out_path_cand)
        if h_c != info["expected_hash"]:
            all_hashes_ok = False
            print(f"Pair {i+1}: CANDIDATE HASH MISMATCH! ({h_c})")

        cand_times.append(t_c)
        base_times.append(t_b)
        cand_peaks.append(peak_c)
        base_peaks.append(peak_b)

        ratio = (t_b - t_c) / t_b  # positive = candidate faster
        pair_ratios.append(ratio)
        if t_c < t_b:
            cand_wins += 1

        print(f"  Pair {i+1:2d}: Base={t_b*1000:6.1f}ms (Peak={peak_b:5.1f}M) | Cand={t_c*1000:6.1f}ms (Peak={peak_c:5.1f}M) | Diff={ratio*100:+5.2f}% [{'WIN' if t_c < t_b else 'LOSS'}]")

    # Statistics
    med_t_cand = sorted(cand_times)[len(cand_times)//2]
    med_t_base = sorted(base_times)[len(base_times)//2]
    med_p_cand = sorted(cand_peaks)[len(cand_peaks)//2]
    med_p_base = sorted(base_peaks)[len(base_peaks)//2]
    med_paired_diff = sorted(pair_ratios)[len(pair_ratios)//2]

    base_spread = (max(base_times) - min(base_times)) / med_t_base * 100.0

    r_t = BOARD_RATIOS[key]["r_T"]
    r_m = BOARD_RATIOS[key]["r_M"]
    r_s = BOARD_RATIOS[key]["r_S"]
    K_base, pts_base = score_track(key, med_t_base * r_t, med_p_base * r_m, base_lines * r_s)
    K_cand, pts_cand = score_track(key, med_t_cand * r_t, med_p_cand * r_m, cand_lines * r_s)
    pts_delta = pts_cand - pts_base

    accepted = (all_hashes_ok and med_paired_diff >= 0.02 and cand_wins >= 12)

    print("\n" + "=" * 80)
    print("PAIRED BENCHMARK RESULTS")
    print("=" * 80)
    print(f"Median Baseline T:  {med_t_base*1000:.1f} ms  (Peak: {med_p_base:.1f} MB, S: {base_lines}) -> Board Est: {pts_base:.2f} pts")
    print(f"Median Candidate T: {med_t_cand*1000:.1f} ms  (Peak: {med_p_cand:.1f} MB, S: {cand_lines}) -> Board Est: {pts_cand:.2f} pts ({pts_delta:+.2f} pts)")
    print(f"Paired Median Improvement: {med_paired_diff*100:+.2f}%")
    print(f"Candidate Win Rate: {cand_wins}/{pairs} pairs ({(cand_wins/pairs)*100:.1f}%)")
    print(f"Baseline Run-to-Run Spread (max-min)/med: {base_spread:.2f}%")
    print(f"All Hashes Byte-Exact: {'PASS' if all_hashes_ok else 'FAIL'}")
    print(f"ACCEPTANCE VERDICT: {'ACCEPTED' if accepted else 'REJECTED (revert recommended)'}")
    print("=" * 80)

    return {
        "accepted": accepted,
        "med_t_cand": med_t_cand,
        "med_t_base": med_t_base,
        "med_p_cand": med_p_cand,
        "med_p_base": med_p_base,
        "cand_lines": cand_lines,
        "base_lines": base_lines,
        "med_paired_diff": med_paired_diff,
        "cand_wins": cand_wins,
        "base_spread": base_spread,
        "all_hashes_ok": all_hashes_ok,
        "pts_base": pts_base,
        "pts_cand": pts_cand,
        "pts_delta": pts_delta
    }

def main():
    if len(sys.argv) >= 4 and sys.argv[1] == "--paired":
        key = sys.argv[2].lower().replace("track-", "")
        cand_dir = sys.argv[3]
        base_dir = sys.argv[4] if len(sys.argv) > 4 else os.path.join(ROOT, f"../wt-base-ab/{TRACKS[key]['dir']}")
        run_paired_comparison(key, cand_dir, base_dir)
        return

    tracks_to_run = sys.argv[1:] if len(sys.argv) > 1 else ["b", "c", "a"]
    print("=" * 80)
    print("SPEED DEMON 2026 BENCHMARK EVALUATOR")
    print("=" * 80)

    results = {}
    for key in tracks_to_run:
        key = key.lower().replace("track-", "")
        if key in TRACKS:
            res = evaluate_track(key)
            if res:
                results[key] = res

    print("\n" + "=" * 80)
    print("BENCHMARK SUMMARY (Median of middle 5 runs)")
    print("=" * 80)
    print(f"| Track | Hash | Time (T) | Peak RAM (M) | Lines (S) | K | Local Pts | Board Est Pts | Tier |")
    print(f"| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |")
    for key, r in results.items():
        h = "PASS" if r["hash_ok"] else "FAIL"
        print(f"| {r['name']} | {h} | {r['T_s']*1000:.1f} ms | {r['M_mb']:.1f} MB | {r['S']} | {r['K']:.2f} | {r['pts_local']:.2f} | {r['pts_board_est']:.2f} | {r['tier']} |")

    if len(results) == 3:
        pts = [r["pts_board_est"] for r in results.values()]
        geom = (pts[0] * pts[1] * pts[2]) ** (1.0 / 3.0)
        print("-" * 80)
        print(f"ESTIMATED BOARD GEOMETRIC MEAN: {geom:.2f} pts")
        print("-" * 80)

if __name__ == "__main__":
    main()
