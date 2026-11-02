use std::fs::File;
use std::os::unix::fs::FileExt;
use std::collections::HashSet;
use std::arch::x86_64::*;

#[inline(always)]
unsafe fn hash_byte_crc(crc1: &mut u64, crc2: &mut u64, b: u8) {
    *crc1 = _mm_crc32_u8(*crc1 as u32, b) as u64;
    *crc2 = _mm_crc32_u8(*crc2 as u32, b) as u64;
}

#[inline(always)]
unsafe fn hash_u64_crc(crc1: &mut u64, crc2: &mut u64, w: u64) {
    *crc1 = _mm_crc32_u64(*crc1, w);
    *crc2 = _mm_crc32_u64(*crc2, w.rotate_left(17));
}

fn main() {
    let path = "/tmp/sd-full-c/records.csv";
    let file = File::open(path).unwrap();
    let flen = file.metadata().unwrap().len() as usize;

    let mut buf = vec![0u8; 1024 * 1024];
    let mut names = HashSet::new();
    let mut hashes = HashSet::new();

    let mut cur_offset = 0;
    let mut rem_len = 0;
    let mut total_records = 0;

    // Skip header
    file.read_exact_at(&mut buf[..128], 0).unwrap();
    let mut p = if buf.starts_with(b"\xef\xbb\xbf") { 3 } else { 0 };
    while p < 128 && buf[p] != b'\n' { p += 1; }
    cur_offset = p + 1;

    let mut chunk_buf = vec![0u8; 1024 * 1024 + 8192];
    while cur_offset < flen || rem_len > 0 {
        let to_read = (1024 * 1024 - rem_len).min(flen.saturating_sub(cur_offset));
        let read_bytes = if to_read > 0 {
            file.read_exact_at(&mut chunk_buf[rem_len..rem_len + to_read], cur_offset as u64).unwrap();
            cur_offset += to_read;
            to_read
        } else { 0 };
        let total = rem_len + read_bytes;
        if total == 0 { break; }
        let mut last_nl = total;
        while last_nl > 0 && chunk_buf[last_nl - 1] != b'\n' { last_nl -= 1; }
        let (slice, mut pos) = (&chunk_buf[..last_nl], 0);

        while pos < last_nl {
            if slice[pos] == 0xef && pos + 3 <= last_nl && &slice[pos..pos + 3] == b"\xef\xbb\xbf" { pos += 3; }
            if pos >= last_nl { break; }
            // Skip id
            while pos < last_nl && slice[pos] != b',' { pos += 1; }
            pos += 1;

            // Parse Name
            let mut crc1 = 0u64;
            let mut crc2 = 0x517cc1b727220a95u64;
            let mut raw_name = Vec::new();

            if pos < last_nl && slice[pos] == b'\"' {
                pos += 1;
                loop {
                    while pos + 8 <= last_nl {
                        let w = unsafe { (slice.as_ptr().add(pos) as *const u64).read_unaligned() };
                        let has_q = (w ^ 0x2222222222222222u64).wrapping_sub(0x0101010101010101u64) & !(w ^ 0x2222222222222222u64) & 0x8080808080808080u64;
                        if has_q != 0 {
                            let off = (has_q.trailing_zeros() >> 3) as usize;
                            for k in 0..off {
                                let b = slice[pos + k];
                                raw_name.push(b);
                                unsafe { hash_byte_crc(&mut crc1, &mut crc2, b); }
                            }
                            pos += off;
                            break;
                        }
                        raw_name.extend_from_slice(&slice[pos..pos + 8]);
                        unsafe { hash_u64_crc(&mut crc1, &mut crc2, w); }
                        pos += 8;
                    }
                    if pos >= last_nl { break; }
                    if slice[pos] == b'\"' {
                        if pos + 1 < last_nl && slice[pos + 1] == b'\"' {
                            raw_name.push(b'\"');
                            unsafe { hash_byte_crc(&mut crc1, &mut crc2, b'\"'); }
                            pos += 2;
                        } else {
                            pos += 1;
                            break;
                        }
                    } else {
                        raw_name.push(slice[pos]);
                        unsafe { hash_byte_crc(&mut crc1, &mut crc2, slice[pos]); }
                        pos += 1;
                    }
                }
            } else {
                while pos < last_nl && slice[pos] != b',' {
                    raw_name.push(slice[pos]);
                    unsafe { hash_byte_crc(&mut crc1, &mut crc2, slice[pos]); }
                    pos += 1;
                }
            }
            pos += 1; // skip comma

            let h = (crc1 << 32) | (crc2 as u32 as u64);
            names.insert(raw_name);
            hashes.insert(h);
            total_records += 1;

            // Skip note
            if pos < last_nl && slice[pos] == b'\"' {
                pos += 1;
                while pos < last_nl {
                    if slice[pos] == b'\"' {
                        if pos + 1 < last_nl && slice[pos + 1] == b'\"' { pos += 2; }
                        else { pos += 1; break; }
                    } else { pos += 1; }
                }
            } else {
                while pos < last_nl && slice[pos] != b',' { pos += 1; }
            }
            pos += 1;

            // Skip region and amount
            while pos < last_nl && slice[pos] != b'\n' { pos += 1; }
            if pos < last_nl && slice[pos] == b'\n' { pos += 1; }
        }

        rem_len = total - last_nl;
        if rem_len > 0 { chunk_buf.copy_within(last_nl..total, 0); }
    }

    println!("Total records verified: {}", total_records);
    println!("Distinct names: {}", names.len());
    println!("Distinct hashes: {}", hashes.len());
    assert_eq!(names.len(), hashes.len(), "COLLISION DETECTED!");
    println!("PERFECT: ZERO COLLISIONS ACROSS ALL RECORDS!");
}
