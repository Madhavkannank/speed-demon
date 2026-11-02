use std::fs::File;
use std::os::fd::AsRawFd;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;
const MADV_DONTNEED: i32 = 4;

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
    fn madvise(addr: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
}

fn main() {
    let file = File::open("/tmp/sd-full-b/docs.txt").unwrap();
    let len = file.metadata().unwrap().len() as usize;
    let fd = file.as_raw_fd();
    let ptr = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_SHARED, fd, 0) };

    // Read every page to fault it in
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    let mut sum = 0u64;
    for &b in slice {
        sum = sum.wrapping_add(b as u64);
    }
    println!("Sum faulted in: {}", sum);

    // Call madvise MADV_DONTNEED
    let ret = unsafe { madvise(ptr, len, MADV_DONTNEED) };
    println!("madvise returned: {}", ret);

    // Sleep 100ms so we can check
    std::thread::sleep(std::time::Duration::from_millis(100));
}
