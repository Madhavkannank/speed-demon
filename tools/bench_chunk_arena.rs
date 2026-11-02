use std::time::Instant;

#[derive(Clone, Copy)]
#[repr(C)]
struct Chunk {
    next: u32,
    len: u8,
    data: [u8; 11],
}

struct WorkerArena {
    heads: Vec<u32>,
    tails: Vec<u32>,
    last_doc: Vec<u32>,
    chunks: Vec<Chunk>,
}

impl WorkerArena {
    fn new(vocab_size: usize) -> Self {
        Self {
            heads: vec![u32::MAX; vocab_size],
            tails: vec![u32::MAX; vocab_size],
            last_doc: vec![u32::MAX; vocab_size],
            chunks: Vec::with_capacity(250_000),
        }
    }

    #[inline(always)]
    fn add_posting(&mut self, term_id: u32, doc_id: u32) {
        let tid = term_id as usize;
        let last = self.last_doc[tid];
        if last == doc_id {
            return;
        }
        let delta = if last == u32::MAX { doc_id } else { doc_id - last - 1 };
        self.last_doc[tid] = doc_id;

        // Encode delta in 1 or 2 bytes
        let mut d_bytes = [0u8; 4];
        let mut d_len = 0;
        let mut v = delta;
        while v >= 128 {
            d_bytes[d_len] = (v & 0x7F) as u8 | 0x80;
            v >>= 7;
            d_len += 1;
        }
        d_bytes[d_len] = v as u8;
        d_len += 1;

        let tail = self.tails[tid];
        if tail != u32::MAX {
            let chunk = unsafe { self.chunks.get_unchecked_mut(tail as usize) };
            let rem = 11 - chunk.len as usize;
            if rem >= d_len {
                for i in 0..d_len {
                    chunk.data[chunk.len as usize + i] = d_bytes[i];
                }
                chunk.len += d_len as u8;
                return;
            }
        }

        // Allocate new chunk
        let new_idx = self.chunks.len() as u32;
        let mut chunk = Chunk {
            next: u32::MAX,
            len: d_len as u8,
            data: [0; 11],
        };
        for i in 0..d_len {
            chunk.data[i] = d_bytes[i];
        }
        self.chunks.push(chunk);

        if tail != u32::MAX {
            unsafe {
                self.chunks.get_unchecked_mut(tail as usize).next = new_idx;
            }
            self.tails[tid] = new_idx;
        } else {
            self.heads[tid] = new_idx;
            self.tails[tid] = new_idx;
        }
    }
}

fn main() {
    let vocab_size = 53444;
    let n = 1_850_000usize;
    let t0 = Instant::now();
    let mut arena = WorkerArena::new(vocab_size);

    for i in 0..n {
        let term_id = (i % vocab_size) as u32;
        let doc_id = (i / 100) as u32;
        arena.add_posting(term_id, doc_id);
    }
    let elapsed = t0.elapsed();

    println!("WorkerArena added 1.85M postings: {:.2} ms", elapsed.as_secs_f64() * 1000.0);
    println!("Total chunks allocated: {} (size: {:.2} MB)",
        arena.chunks.len(), (arena.chunks.len() * std::mem::size_of::<Chunk>()) as f64 / 1e6);

    // Test iteration of a term
    let t1 = Instant::now();
    let mut decoded = Vec::new();
    let mut cur = arena.heads[1234];
    let mut last = 0u32;
    while cur != u32::MAX {
        let chunk = &arena.chunks[cur as usize];
        let mut p = 0;
        let len = chunk.len as usize;
        while p < len {
            let mut delta = 0u32;
            let mut shift = 0;
            loop {
                let b = chunk.data[p];
                p += 1;
                delta |= ((b & 0x7F) as u32) << shift;
                if b < 128 { break; }
                shift += 7;
            }
            let doc_id = if decoded.is_empty() { delta } else { last + delta + 1 };
            decoded.push(doc_id);
            last = doc_id;
        }
        cur = chunk.next;
    }
    println!("Decoded term 1234 ({} docs) in {:.3} ms", decoded.len(), t1.elapsed().as_secs_f64() * 1000.0);
}
