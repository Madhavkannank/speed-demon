use std::fs::File;
use std::os::unix::fs::FileExt;
use std::time::Instant;

fn main() {
    let path = "/tmp/sd-full-a/requests.log";
    let file = File::open(path).unwrap();
    let len = file.metadata().unwrap().len() as usize;
    println!("File open ok, len: {}", len);
}
