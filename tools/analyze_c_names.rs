use std::fs::File;
use std::os::fd::AsRawFd;
use std::collections::HashSet;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
}

fn main() {
    let file = File::open("/tmp/sd-full-c/records.csv").unwrap();
    let flen = file.metadata().unwrap().len() as usize;
    let ptr = unsafe { mmap(std::ptr::null_mut(), flen, 1, 1, file.as_raw_fd(), 0) as *const u8 };
    let slice = unsafe { std::slice::from_raw_parts(ptr, flen) };

    // Load regions
    let r_bytes = std::fs::read("/tmp/sd-full-c/regions.csv").unwrap();
    let mut reg_map = vec![0u8; 1_000_000];
    let mut p = 0;
    while p < r_bytes.len() && r_bytes[p] != b'\n' { p += 1; }
    if p < r_bytes.len() { p += 1; }
    while p < r_bytes.len() {
        if r_bytes[p] == b'\r' || r_bytes[p] == b'\n' { p += 1; continue; }
        let mut rid = 0usize;
        while p < r_bytes.len() && r_bytes[p] >= b'0' && r_bytes[p] <= b'9' {
            rid = rid * 10 + (r_bytes[p] - b'0') as usize;
            p += 1;
        }
        if p < r_bytes.len() && r_bytes[p] == b',' { p += 1; }
        let c_start = p;
        while p < r_bytes.len() && r_bytes[p] != b'\r' && r_bytes[p] != b'\n' { p += 1; }
        let c_slice = &r_bytes[c_start..p];
        if p < r_bytes.len() && r_bytes[p] == b'\r' { p += 1; }
        if p < r_bytes.len() && r_bytes[p] == b'\n' { p += 1; }
        let cid = c_slice[0] as u8; // simple
        if rid < 1_000_000 { reg_map[rid] = cid; }
    }

    let mut distinct_names = HashSet::new();
    let mut distinct_country_names = HashSet::new();

    let mut header = 0;
    while header < flen && slice[header] != b'\n' { header += 1; }
    header += 1;

    let mut p = header;
    let p_raw = slice.as_ptr();

    while p < flen {
        unsafe {
            if *p_raw.add(p) == 0xef && p + 3 <= flen && *p_raw.add(p + 1) == 0xbb && *p_raw.add(p + 2) == 0xbf { p += 3; }
            if p >= flen { break; }
            while *p_raw.add(p) != b',' { p += 1; }
            p += 1; // skip ID

            // name
            let n_start;
            let raw_len;
            if *p_raw.add(p) == b'\"' {
                p += 1;
                n_start = p;
                loop {
                    while *p_raw.add(p) != b'\"' { p += 1; }
                    if *p_raw.add(p + 1) == b'\"' { p += 2; } else { break; }
                }
                raw_len = p - n_start;
                p += 1;
            } else {
                n_start = p;
                while *p_raw.add(p) != b',' { p += 1; }
                raw_len = p - n_start;
            }
            p += 1; // skip comma

            let name_slice = std::slice::from_raw_parts(p_raw.add(n_start), raw_len);

            // note
            if *p_raw.add(p) == b'\"' {
                p += 1;
                loop {
                    while *p_raw.add(p) != b'\"' { p += 1; }
                    if *p_raw.add(p + 1) == b'\"' { p += 2; } else { break; }
                }
                p += 1;
            } else {
                while *p_raw.add(p) != b',' { p += 1; }
            }
            p += 1;

            let mut reg_id = 0usize;
            while *p_raw.add(p) != b',' {
                reg_id = reg_id * 10 + (*p_raw.add(p) - b'0') as usize;
                p += 1;
            }
            p += 1;

            let cid = if reg_id < 1_000_000 { reg_map[reg_id] } else { 0 };

            // amount
            while *p_raw.add(p) != b'\n' { p += 1; }
            p += 1;

            distinct_names.insert(name_slice);
            distinct_country_names.insert((cid, name_slice));
        }
    }

    println!("Total distinct names: {}", distinct_names.len());
    println!("Total distinct (country, name) pairs: {}", distinct_country_names.len());
    unsafe { munmap(ptr as *mut std::ffi::c_void, flen); }
}
