use std::time::Instant;

#[derive(Clone, Copy)]
#[repr(C)]
struct TableSlot {
    tag: u32,
    id: u32,
}

#[derive(Clone, Copy)]
struct TermEntry {
    offset: u32,
    len: u16,
}

struct FastTermTable {
    slots: Vec<TableSlot>,
    mask: usize,
    arena: Vec<u8>,
    entries: Vec<TermEntry>,
}

#[inline(always)]
fn hash32(b: &[u8]) -> u32 {
    let mut h = 0x517cc1b727220a95u64;
    for &x in b {
        h = (h.rotate_left(5) ^ (x as u64)).wrapping_mul(0x9e3779b97f4a7c15);
    }
    (h ^ (h >> 32)) as u32
}

impl FastTermTable {
    fn with_capacity(cap: usize) -> Self {
        assert!(cap.is_power_of_two());
        Self {
            slots: vec![TableSlot { tag: u32::MAX, id: u32::MAX }; cap],
            mask: cap - 1,
            arena: Vec::with_capacity(1024 * 1024),
            entries: Vec::with_capacity(100000),
        }
    }

    #[inline(always)]
    fn get_or_insert(&mut self, term: &[u8]) -> u32 {
        let h = hash32(term);
        let tag = if h == u32::MAX { 0 } else { h };
        let mut idx = (tag as usize) & self.mask;
        loop {
            let slot = unsafe { self.slots.get_unchecked(idx) };
            if slot.tag == u32::MAX {
                // Empty slot -> insert
                let id = self.entries.len() as u32;
                let offset = self.arena.len() as u32;
                self.arena.extend_from_slice(term);
                self.entries.push(TermEntry { offset, len: term.len() as u16 });
                unsafe {
                    *self.slots.get_unchecked_mut(idx) = TableSlot { tag, id };
                }
                return id;
            }
            if slot.tag == tag {
                let entry = unsafe { *self.entries.get_unchecked(slot.id as usize) };
                if entry.len as usize == term.len() {
                    let s = unsafe {
                        std::slice::from_raw_parts(
                            self.arena.as_ptr().add(entry.offset as usize),
                            entry.len as usize,
                        )
                    };
                    if s == term {
                        return slot.id;
                    }
                }
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn lookup(&self, term: &[u8]) -> Option<u32> {
        let h = hash32(term);
        let tag = if h == u32::MAX { 0 } else { h };
        let mut idx = (tag as usize) & self.mask;
        loop {
            let slot = unsafe { *self.slots.get_unchecked(idx) };
            if slot.tag == u32::MAX {
                return None;
            }
            if slot.tag == tag {
                let entry = unsafe { *self.entries.get_unchecked(slot.id as usize) };
                if entry.len as usize == term.len() {
                    let s = unsafe {
                        std::slice::from_raw_parts(
                            self.arena.as_ptr().add(entry.offset as usize),
                            entry.len as usize,
                        )
                    };
                    if s == term {
                        return Some(slot.id);
                    }
                }
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

enum FastParsedQuery {
    Bool {
        first_term: u32,
        ops: Vec<(u8, u32)>, // op: 0=AND, 1=OR, 2=NOT
    },
    Phrase {
        term_ids: Vec<u32>,
        words_offsets: Vec<(u32, u16)>, // offset and len in arena
    },
    Empty,
}

fn main() {
    let queries_path = "/tmp/sd-full-b/queries.txt";
    let q_bytes = std::fs::read(queries_path).unwrap();

    let t0 = Instant::now();
    let mut table = FastTermTable::with_capacity(262144); // 1 MB table!
    let mut parsed_queries = Vec::with_capacity(50000);

    let mut p = 0;
    let len = q_bytes.len();
    while p < len {
        let line_start = p;
        while p < len && q_bytes[p] != b'\n' {
            p += 1;
        }
        let line = &q_bytes[line_start..p];
        if p < len && q_bytes[p] == b'\n' {
            p += 1;
        }
        if line.is_empty() {
            continue;
        }

        if line.starts_with(b"BOOL ") {
            let rest = &line[5..];
            let mut parts = rest.split(|&b| b == b' ');
            if let Some(first) = parts.next() {
                let first_id = table.get_or_insert(first);
                let mut ops = Vec::new();
                while let Some(op_bytes) = parts.next() {
                    if let Some(term_bytes) = parts.next() {
                        let op_code = match op_bytes {
                            b"OR" => 1,
                            b"NOT" => 2,
                            _ => 0,
                        };
                        let tid = table.get_or_insert(term_bytes);
                        ops.push((op_code, tid));
                    }
                }
                parsed_queries.push(FastParsedQuery::Bool { first_term: first_id, ops });
            } else {
                parsed_queries.push(FastParsedQuery::Empty);
            }
        } else if line.starts_with(b"PHRASE \"") {
            let mut inner = &line[8..];
            if inner.ends_with(b"\"") {
                inner = &inner[..inner.len() - 1];
            }
            let mut term_ids = Vec::new();
            let mut words_offsets = Vec::new();
            for word in inner.split(|&b| b == b' ') {
                if !word.is_empty() {
                    let tid = table.get_or_insert(word);
                    let entry = table.entries[tid as usize];
                    term_ids.push(tid);
                    words_offsets.push((entry.offset, entry.len));
                }
            }
            parsed_queries.push(FastParsedQuery::Phrase { term_ids, words_offsets });
        } else {
            parsed_queries.push(FastParsedQuery::Empty);
        }
    }
    let elapsed = t0.elapsed();

    println!("Single-pass FastTermTable + ParsedQueries:");
    println!("Time: {:.2} ms", elapsed.as_secs_f64() * 1000.0);
    println!("Unique terms: {}", table.entries.len());
    println!("Arena size: {} bytes", table.arena.len());
    println!("Parsed queries: {}", parsed_queries.len());

    // Test lookup speed: lookup 2 million times
    let t_lookup = Instant::now();
    let mut hits = 0;
    for entry in &table.entries {
        let term = &table.arena[entry.offset as usize..entry.offset as usize + entry.len as usize];
        if let Some(_id) = table.lookup(term) {
            hits += 1;
        }
    }
    println!("Lookup all {} terms: {:.3} ms (hits: {})", table.entries.len(), t_lookup.elapsed().as_secs_f64() * 1000.0, hits);
}
