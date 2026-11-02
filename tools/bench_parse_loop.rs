use std::fs::File;
use std::os::unix::fs::FileExt;
use std::time::Instant;
use std::arch::x86_64::*;

const TABLE_SIZE: usize = 4096;
const TABLE_MASK: usize = TABLE_SIZE - 1;

#[derive(Clone, Copy)]
#[repr(C)]
struct HashSlot {
    hash: u32,
    id: u16,
    len: u16,
}

#[inline(always)]
unsafe fn crc32_hash(slice: &[u8]) -> u32 {
    let mut crc = 0u64;
    let mut p = slice.as_ptr();
    let mut len = slice.len();
    while len >= 8 {
        let w = (p as *const u64).read_unaligned();
        crc = _mm_crc32_u64(crc, w);
        p = p.add(8);
        len -= 8;
    }
    if len > 0 {
        let mut rem = [0u8; 8];
        std::ptr::copy_nonoverlapping(p, rem.as_mut_ptr(), len);
        let w = u64::from_le_bytes(rem);
        crc = _mm_crc32_u64(crc, w);
    }
    crc as u32
}

#[inline(always)]
unsafe fn find_newline_avx2(buf: *const u8, mut pos: usize, max: usize) -> usize {
    let target = _mm256_set1_epi8(b'\n' as i8);
    while pos + 32 <= max {
        let chunk = _mm256_loadu_si256(buf.add(pos) as *const __m256i);
        let cmp = _mm256_cmpeq_epi8(chunk, target);
        let mask = _mm256_movemask_epi8(cmp);
        if mask != 0 {
            return pos + mask.trailing_zeros() as usize;
        }
        pos += 32;
    }
    while pos < max && *buf.add(pos) != b'\n' {
        pos += 1;
    }
    pos
}

fn main() {
    let path = "/tmp/sd-full-a/requests.log";
    let file = File::open(path).unwrap();
    let len = 17 * 1024 * 1024;
    let mut chunk = vec![0u8; len];
    file.read_exact_at(&mut chunk, 0).unwrap();

    let mut last_nl = len;
    while last_nl > 0 && chunk[last_nl - 1] != b'\n' { last_nl -= 1; }
    let slice = &chunk[..last_nl];

    // Benchmark 4: Exact same operations as solution worker
    let mut local_table = vec![HashSlot { hash: 0, id: 0, len: 0 }; TABLE_SIZE];
    let mut local_eps: Vec<Vec<u8>> = Vec::with_capacity(2048);
    let mut raw_gids: Vec<u16> = Vec::with_capacity(260_000);
    let mut raw_lats: Vec<u32> = Vec::with_capacity(260_000);
    let mut errors = vec![0u32; 16384];
    let mut group_counts = vec![0u32; 16385];

    let t0 = Instant::now();
    let (mut pos, mut last_hour_str, mut cached_hour_base) = (0, [0u8; 13], 0usize);
    while pos + 35 <= last_nl {
        let hour_prefix = unsafe { slice.get_unchecked(pos..pos + 13) };
        let hour_base = if hour_prefix == &last_hour_str { cached_hour_base } else {
            last_hour_str.copy_from_slice(hour_prefix);
            cached_hour_base = 0;
            0
        };
        let ep_start = match unsafe { *slice.get_unchecked(pos + 28) } {
            b'G' => pos + 32,
            b'P' => if unsafe { *slice.get_unchecked(pos + 29) } == b'U' { pos + 32 }
                    else if unsafe { *slice.get_unchecked(pos + 29) } == b'O' { pos + 33 }
                    else { pos + 34 },
            b'H' => pos + 33,
            b'D' => pos + 35,
            _ => pos + 36,
        };
        let nl = unsafe { find_newline_avx2(slice.as_ptr(), pos + 39, last_nl) };
        let mut p_sp = nl - 1;
        let mut lat = 0u32;
        let mut mult = 1u32;
        while unsafe { *slice.get_unchecked(p_sp) } != b' ' {
            lat += (unsafe { *slice.get_unchecked(p_sp) } - b'0') as u32 * mult;
            mult *= 10;
            p_sp -= 1;
        }
        let is_5xx = (unsafe { *slice.get_unchecked(p_sp - 3) } == b'5') as u32;
        let ep_len = (p_sp - 4) - ep_start;
        let ep_slice = unsafe { slice.get_unchecked(ep_start..p_sp - 4) };

        let h = unsafe { crc32_hash(ep_slice) };
        let mut slot = (h as usize) & TABLE_MASK;
        let ep_id = loop {
            let entry = unsafe { local_table.get_unchecked(slot) };
            if entry.hash == h && entry.len as usize == ep_len {
                if unsafe { local_eps.get_unchecked(entry.id as usize) } == ep_slice {
                    break entry.id as usize;
                }
            }
            if entry.len == 0 {
                let new_id = local_eps.len() as u16;
                unsafe {
                    *local_table.get_unchecked_mut(slot) = HashSlot {
                        hash: h,
                        id: new_id,
                        len: ep_len as u16,
                    };
                }
                local_eps.push(ep_slice.to_vec());
                break new_id as usize;
            }
            slot = (slot + 1) & TABLE_MASK;
        };

        pos = nl + 1;
        let g_id = hour_base + ep_id;
        unsafe {
            *errors.get_unchecked_mut(g_id) += is_5xx;
            *group_counts.get_unchecked_mut(g_id + 1) += 1;
        }
        raw_gids.push(g_id as u16);
        raw_lats.push(lat);
    }
    let d = t0.elapsed();
    println!("Exact worker loop: {:?}, raw_recs: {}", d, raw_gids.len());
}
