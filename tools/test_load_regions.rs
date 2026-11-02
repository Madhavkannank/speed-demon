use std::fs;
use std::path::Path;
use std::time::Instant;

fn load_regions(path: &Path) {
    let t0 = Instant::now();
    let bytes = fs::read(path).unwrap();
    let t_read = t0.elapsed();
    let mut region_map = vec![0u8; 1_000_000];
    let t_alloc = t0.elapsed();
    let mut country_names = vec!["_UNMATCHED".to_string()];
    let mut country_table = vec![(0u32, 0u8); 1024];
    let len = bytes.len();
    let mut p = 0;
    while p < len && bytes[p] != b'\n' {
        p += 1;
    }
    if p < len {
        p += 1;
    }
    while p < len {
        if bytes[p] == b'\r' || bytes[p] == b'\n' {
            p += 1;
            continue;
        }
        let mut reg_id = 0usize;
        while p < len && bytes[p] >= b'0' && bytes[p] <= b'9' {
            reg_id = reg_id * 10 + (bytes[p] - b'0') as usize;
            p += 1;
        }
        if reg_id >= region_map.len() {
            region_map.resize(reg_id + 1, 0);
        }
        if p < len && bytes[p] == b',' {
            p += 1;
        }
        let c_start = p;
        let mut h = 0x517cc1b7u32;
        while p < len && bytes[p] != b'\r' && bytes[p] != b'\n' {
            h = (h.rotate_left(5) ^ (bytes[p] as u32)).wrapping_mul(0x9e3779b9);
            p += 1;
        }
        let c_slice = &bytes[c_start..p];
        if p < len && bytes[p] == b'\r' {
            p += 1;
        }
        if p < len && bytes[p] == b'\n' {
            p += 1;
        }
        let mut slot = (h as usize) & 1023;
        let c_id = loop {
            let (sh, id) = country_table[slot];
            if sh == h && country_names[id as usize].as_bytes() == c_slice {
                break id;
            }
            if sh == 0 {
                let id = country_names.len() as u8;
                country_names.push(String::from_utf8_lossy(c_slice).to_string());
                country_table[slot] = (h, id);
                break id;
            }
            slot = (slot + 1) & 1023;
        };
        if reg_id < 1_000_000 {
            region_map[reg_id] = c_id;
        }
    }
    let t_parse = t0.elapsed();
    println!(
        "read={:?}, alloc={:?}, parse={:?}, total={:?}",
        t_read,
        t_alloc - t_read,
        t_parse - t_alloc,
        t_parse
    );
}

fn main() {
    let p = Path::new("/tmp/sd-full-c/regions.csv");
    for _ in 0..5 {
        load_regions(p);
    }
}
