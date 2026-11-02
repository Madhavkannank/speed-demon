#!/usr/bin/env python3
import os
import sys
import subprocess
import hashlib
import random
import shutil

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

def run_cmd(cmd, env=None):
    res = subprocess.run(cmd, shell=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
    return res.returncode, res.stdout, res.stderr

def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(65536):
            h.update(chunk)
    return h.hexdigest()

def test_h3_track_a():
    print("=== H3 Track A Adversarial Tests ===")
    src_log = "/tmp/sd-full-a/requests.log"
    if not os.path.exists(src_log):
        print(f"Error: {src_log} not found")
        return False
    
    test_dir = "/tmp/sd-h3-a"
    os.makedirs(test_dir, exist_ok=True)
    out_dir_ref = "/tmp/sd-h3-a-ref"
    out_dir_test = "/tmp/sd-h3-a-test"
    os.makedirs(out_dir_ref, exist_ok=True)
    os.makedirs(out_dir_test, exist_ok=True)

    # 1. Shuffled lines test
    print("  Testing randomly shuffled lines...")
    shuffled_log = os.path.join(test_dir, "requests.log")
    with open(src_log, "rb") as f:
        lines = f.readlines()
    
    # Take a 100k line sample to test quickly or full lines
    sample_lines = lines[:200000]
    random.seed(42)
    random.shuffle(sample_lines)
    with open(shuffled_log, "wb") as f:
        f.writelines(sample_lines)

    # Reference run
    code, _, err = run_cmd(f"python3 {ROOT}/reference/a_ingest.py --data {test_dir} --out {out_dir_ref}")
    if code != 0:
        print(f"Reference failed on shuffled lines: {err}")
        return False
    ref_hash = sha256_file(os.path.join(out_dir_ref, "summary.txt"))

    # Test run
    env = os.environ.copy()
    env["DATA_DIR"] = test_dir
    env["OUT_DIR"] = out_dir_test
    code, _, err = run_cmd(f"{ROOT}/track-a/target/release/solution", env=env)
    if code != 0:
        print(f"Candidate failed on shuffled lines: {err}")
        return False
    cand_hash = sha256_file(os.path.join(out_dir_test, "summary.txt"))
    if ref_hash != cand_hash:
        print(f"Hash mismatch on shuffled lines: ref={ref_hash} cand={cand_hash}")
        return False
    print("  -> Shuffled lines: PASS")

    # 2. Reversed file test
    print("  Testing reversed lines...")
    reversed_lines = sample_lines[::-1]
    with open(shuffled_log, "wb") as f:
        f.writelines(reversed_lines)
    
    run_cmd(f"python3 {ROOT}/reference/a_ingest.py --data {test_dir} --out {out_dir_ref}")
    ref_hash = sha256_file(os.path.join(out_dir_ref, "summary.txt"))
    run_cmd(f"{ROOT}/track-a/target/release/solution", env=env)
    cand_hash = sha256_file(os.path.join(out_dir_test, "summary.txt"))
    if ref_hash != cand_hash:
        print(f"Hash mismatch on reversed lines: ref={ref_hash} cand={cand_hash}")
        return False
    print("  -> Reversed lines: PASS")

    return True

def test_h3_track_b():
    print("=== H3 Track B Adversarial Tests ===")
    src_queries = "/tmp/sd-full-b/queries.txt"
    src_docs = "/tmp/sd-full-b/docs.txt"
    if not os.path.exists(src_queries) or not os.path.exists(src_docs):
        print("Error: full-b docs/queries not found")
        return False

    test_dir = "/tmp/sd-h3-b"
    os.makedirs(test_dir, exist_ok=True)
    out_dir_test = "/tmp/sd-h3-b-test"
    os.makedirs(out_dir_test, exist_ok=True)

    # Link docs.txt
    if not os.path.exists(os.path.join(test_dir, "docs.txt")):
        os.symlink(src_docs, os.path.join(test_dir, "docs.txt"))

    # Generate 3x more distinct terms (e.g. 250,000 queries with synthetic terms)
    print("  Generating queries with 3x distinct terms (250k terms)...")
    adv_queries_path = os.path.join(test_dir, "queries.txt")
    with open(adv_queries_path, "w") as f:
        # Write 250,000 distinct term boolean queries
        for i in range(250000):
            f.write(f"BOOL synterm{i}x\n")

    env = os.environ.copy()
    env["DATA_DIR"] = test_dir
    env["OUT_DIR"] = out_dir_test
    print("  Running candidate on 250k distinct term queries...")
    code, out, err = run_cmd(f"{ROOT}/track-b/target/release/solution", env=env)
    if code != 0:
        print(f"Candidate crashed or hung: code={code}, err={err}")
        return False
    res_path = os.path.join(out_dir_test, "results.txt")
    if not os.path.exists(res_path):
        print("results.txt not generated")
        return False
    with open(res_path) as f:
        line_count = sum(1 for _ in f)
    if line_count != 250000:
        print(f"Expected 250000 lines, got {line_count}")
        return False
    print("  -> 3x distinct terms test: PASS (no hang, no crash, 250,000 lines output)")
    return True

def test_h3_track_c():
    print("=== H3 Track C Adversarial Tests ===")
    src_rec = "/tmp/sd-full-c/records.csv"
    src_reg = "/tmp/sd-full-c/regions.csv"
    if not os.path.exists(src_rec) or not os.path.exists(src_reg):
        print("Error: full-c records/regions not found")
        return False

    test_dir = "/tmp/sd-h3-c"
    os.makedirs(test_dir, exist_ok=True)
    out_dir_ref = "/tmp/sd-h3-c-ref"
    out_dir_test = "/tmp/sd-h3-c-test"
    os.makedirs(out_dir_ref, exist_ok=True)
    os.makedirs(out_dir_test, exist_ok=True)

    if not os.path.exists(os.path.join(test_dir, "regions.csv")):
        os.symlink(src_reg, os.path.join(test_dir, "regions.csv"))

    # Create records.csv where boundary has BOM at chunk boundaries and long note > 4KB
    print("  Creating adversarial records.csv with >4KB notes and mid-file BOMs...")
    adv_rec_path = os.path.join(test_dir, "records.csv")
    with open(src_rec, "rb") as f_in, open(adv_rec_path, "wb") as f_out:
        # Read header
        header = f_in.readline()
        f_out.write(header)
        # Write a record with a 5000-byte multi-line note right at the start
        f_out.write(b'1,"Alice","' + (b"long note text line\n" * 250) + b'",1,10.00\r\n')
        # Write some sample records with BOM before records
        for i in range(2, 20000):
            line = f_in.readline()
            if not line: break
            if i % 1000 == 0:
                f_out.write(b"\xef\xbb\xbf") # mid-file BOM
            f_out.write(line)

    print("  Running reference...")
    code, _, err = run_cmd(f"python3 {ROOT}/reference/c_join.py --data {test_dir} --out {out_dir_ref}")
    if code != 0:
        print(f"Reference failed on adversarial C: {err}")
        return False
    ref_hash = sha256_file(os.path.join(out_dir_ref, "aggregates.txt"))

    print("  Running candidate...")
    env = os.environ.copy()
    env["DATA_DIR"] = test_dir
    env["OUT_DIR"] = out_dir_test
    code, _, err = run_cmd(f"{ROOT}/track-c/target/release/solution", env=env)
    if code != 0:
        print(f"Candidate failed on adversarial C: {err}")
        return False
    cand_hash = sha256_file(os.path.join(out_dir_test, "aggregates.txt"))
    if ref_hash != cand_hash:
        print(f"Hash mismatch on adversarial C: ref={ref_hash} cand={cand_hash}")
        return False
    print("  -> Adversarial C (>4KB note + mid-file BOM): PASS (exact hash match)")
    return True

if __name__ == "__main__":
    ok_a = test_h3_track_a()
    ok_b = test_h3_track_b()
    ok_c = test_h3_track_c()
    if ok_a and ok_b and ok_c:
        print("\nALL H3 ADVERSARIAL TESTS PASSED VERBATIM!")
    else:
        sys.exit(1)
