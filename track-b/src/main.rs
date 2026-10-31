use std::{collections::HashMap, fs::{self, File}, hash::{BuildHasherDefault, Hasher}, io::{BufRead, BufReader, BufWriter, Write}, os::unix::fs::FileExt, path::Path, thread};
#[derive(Default)] struct FastHasher(u64); impl Hasher for FastHasher { #[inline(always)] fn finish(&self) -> u64 { self.0 } #[inline(always)] fn write(&mut self, b: &[u8]) { let mut h = self.0.wrapping_add(0x517cc1b727220a95); for &x in b { h = (h.rotate_left(5) ^ (x as u64)).wrapping_mul(0x9e3779b97f4a7c15); } self.0 = h; } }
#[inline(always)] fn hash_bytes(b: &[u8]) -> u64 { let mut h = 0x517cc1b727220a95u64; for &x in b { h = (h.rotate_left(5) ^ (x as u64)).wrapping_mul(0x9e3779b97f4a7c15); } h }
type FastBuildHasher = BuildHasherDefault<FastHasher>; #[inline(always)] fn intersect_sorted(a: &[u32], b: &[u32], out: &mut Vec<u32>) { out.clear(); if a.is_empty() || b.is_empty() { return; }
if b.len() > a.len() * 8 { let mut s = b; for &v in a { match s.binary_search(&v) { Ok(i) => { out.push(v); s = &s[i + 1..]; }, Err(i) => s = &s[i..] } if s.is_empty() { break; } } return; }
if a.len() > b.len() * 8 { let mut s = a; for &v in b { match s.binary_search(&v) { Ok(i) => { out.push(v); s = &s[i + 1..]; }, Err(i) => s = &s[i..] } if s.is_empty() { break; } } return; }
let (mut i, mut j) = (0, 0); while i < a.len() && j < b.len() { if a[i] == b[j] { out.push(a[i]); i += 1; j += 1; } else if a[i] < b[j] { i += 1; } else { j += 1; } } }
#[inline(always)] fn union_sorted(a: &[u32], b: &[u32], out: &mut Vec<u32>) { out.clear(); let (mut i, mut j) = (0, 0); while i < a.len() && j < b.len() { if a[i] == b[j] { out.push(a[i]); i += 1; j += 1; } else if a[i] < b[j] { out.push(a[i]); i += 1; } else { out.push(b[j]); j += 1; } } if i < a.len() { out.extend_from_slice(&a[i..]); } if j < b.len() { out.extend_from_slice(&b[j..]); } }
#[inline(always)] fn difference_sorted(a: &[u32], b: &[u32], out: &mut Vec<u32>) { out.clear(); let (mut i, mut j) = (0, 0); while i < a.len() && j < b.len() { if a[i] < b[j] { out.push(a[i]); i += 1; } else if a[i] == b[j] { i += 1; j += 1; } else { j += 1; } } if i < a.len() { out.extend_from_slice(&a[i..]); } }
#[inline(always)] fn append_u32(buf: &mut Vec<u8>, mut n: u32) { if n == 0 { buf.push(b'0'); return; } let mut t = [0u8; 10]; let mut i = 10; while n > 0 { i -= 1; t[i] = b'0' + (n % 10) as u8; n /= 10; } buf.extend_from_slice(&t[i..]); } #[inline(always)] fn append_u64(buf: &mut Vec<u8>, mut n: u64) { if n == 0 { buf.push(b'0'); return; } let mut t = [0u8; 20]; let mut i = 20; while n > 0 { i -= 1; t[i] = b'0' + (n % 10) as u8; n /= 10; } buf.extend_from_slice(&t[i..]); }
#[inline(always)] fn fast_write_results(buf: &mut Vec<u8>, ids: &[u32]) { if ids.is_empty() { buf.extend_from_slice(b"0 0\n"); return; }
let sum: u64 = ids.iter().map(|&x| x as u64).sum(); append_u32(buf, ids.len() as u32); buf.push(b' '); append_u64(buf, sum); for &id in &ids[..ids.len().min(10)] { buf.push(b' '); append_u32(buf, id); } buf.push(b'\n'); }
enum ParsedQuery { Bool { first_term: Option<u32>, ops: Vec<(u8, Option<u32>)> }, Phrase { term_ids: Vec<u32>, words: Vec<Vec<u8>> }, Empty } struct WorkerIndexData { index: Vec<Vec<u32>>, doc_text_offsets: Vec<u64>, doc_text_lens: Vec<u32> }
fn main() -> std::io::Result<()> {
let (data_dir, out_dir) = (std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string()), std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string()));
let (docs_path, queries_path, out_path) = (Path::new(&data_dir).join("docs.txt"), Path::new(&data_dir).join("queries.txt"), Path::new(&out_dir).join("results.txt"));
let queries_reader = BufReader::with_capacity(1024 * 1024, File::open(&queries_path)?);
let mut vocab: HashMap<Box<[u8]>, u32, FastBuildHasher> = HashMap::with_capacity_and_hasher(32768, FastBuildHasher::default());
let (mut raw_queries, mut bloom) = (Vec::with_capacity(50000), vec![0u64; 32768]);
for line_res in queries_reader.lines() { let line = line_res?; if line.is_empty() { continue; }
if let Some(rest) = line.strip_prefix("BOOL ") { for chunk in rest.split(' ').step_by(2) { let t = chunk.as_bytes(); if !vocab.contains_key(t) { let id = vocab.len() as u32; vocab.insert(t.into(), id); let h = hash_bytes(t);
let (b1, b2, b3, b4) = ((h & 0x1FFFFF) as usize, ((h >> 21) & 0x1FFFFF) as usize, (h.rotate_left(17) & 0x1FFFFF) as usize, (h.rotate_left(37) & 0x1FFFFF) as usize);
bloom[b1 >> 6] |= 1u64 << (b1 & 63); bloom[b2 >> 6] |= 1u64 << (b2 & 63); bloom[b3 >> 6] |= 1u64 << (b3 & 63); bloom[b4 >> 6] |= 1u64 << (b4 & 63); } } }
else if let Some(rest) = line.strip_prefix("PHRASE \"") { for term_str in rest.strip_suffix('"').unwrap_or(rest).split(' ') { let t = term_str.as_bytes(); if !vocab.contains_key(t) { let id = vocab.len() as u32; vocab.insert(t.into(), id); let h = hash_bytes(t);
let (b1, b2, b3, b4) = ((h & 0x1FFFFF) as usize, ((h >> 21) & 0x1FFFFF) as usize, (h.rotate_left(17) & 0x1FFFFF) as usize, (h.rotate_left(37) & 0x1FFFFF) as usize);
bloom[b1 >> 6] |= 1u64 << (b1 & 63); bloom[b2 >> 6] |= 1u64 << (b2 & 63); bloom[b3 >> 6] |= 1u64 << (b3 & 63); bloom[b4 >> 6] |= 1u64 << (b4 & 63); } } }
raw_queries.push(line); }
let mut parsed_queries = Vec::with_capacity(raw_queries.len());
for line in &raw_queries {
if let Some(rest) = line.strip_prefix("BOOL ") { let parts: Vec<&str> = rest.split(' ').collect(); if parts.is_empty() { parsed_queries.push(ParsedQuery::Empty); continue; }
let first_term = vocab.get(parts[0].as_bytes()).copied(); let mut ops = Vec::new();
for chunk in parts[1..].chunks(2) { if chunk.len() < 2 { break; } let op_code = match chunk[0] { "OR" => 1, "NOT" => 2, _ => 0 }; ops.push((op_code, vocab.get(chunk[1].as_bytes()).copied())); }
parsed_queries.push(ParsedQuery::Bool { first_term, ops }); }
else if let Some(rest) = line.strip_prefix("PHRASE \"") { let (mut term_ids, mut words) = (Vec::new(), Vec::new()); for pt in rest.strip_suffix('"').unwrap_or(rest).split(' ') { if let Some(&id) = vocab.get(pt.as_bytes()) { term_ids.push(id); words.push(pt.as_bytes().to_vec()); } } parsed_queries.push(ParsedQuery::Phrase { term_ids, words }); }
else { parsed_queries.push(ParsedQuery::Empty); } }
let (vocab_size, docs_file) = (vocab.len(), File::open(&docs_path)?); let file_len = docs_file.metadata()?.len() as usize;
const NUM_INDEX_WORKERS: usize = 12; let approx_chunk = file_len / NUM_INDEX_WORKERS; let mut chunk_bounds = Vec::with_capacity(NUM_INDEX_WORKERS + 1); chunk_bounds.push(0);
let mut scan_buf = vec![0u8; 4096]; for i in 1..NUM_INDEX_WORKERS { let p = i * approx_chunk; docs_file.read_exact_at(&mut scan_buf, p as u64)?; let mut o = 0; while o < scan_buf.len() && scan_buf[o] != b'\n' { o += 1; } chunk_bounds.push(p + o + 1); } chunk_bounds.push(file_len); drop(scan_buf);
let mut worker_data: Vec<WorkerIndexData> = Vec::with_capacity(NUM_INDEX_WORKERS);
thread::scope(|s| { let (mut handles, bloom_ref, vocab_ref, docs_path_ref) = (Vec::with_capacity(NUM_INDEX_WORKERS), &bloom, &vocab, &docs_path);
for w_idx in 0..NUM_INDEX_WORKERS { let (start_pos, end_pos) = (chunk_bounds[w_idx], chunk_bounds[w_idx + 1]);
handles.push(s.spawn(move || { let (d_file, mut local_index) = (File::open(docs_path_ref).unwrap(), (0..vocab_size).map(|_| Vec::new()).collect::<Vec<Vec<u32>>>());
let (mut last_seen, mut text_offsets, mut text_lens) = (vec![u32::MAX; vocab_size], Vec::with_capacity(45_000), Vec::with_capacity(45_000));
let (mut chunk_buf, mut cur_offset, mut rem_len) = (vec![0u8; 512 * 1024], start_pos, 0);
while cur_offset < end_pos || rem_len > 0 { let to_read = (512 * 1024 - rem_len).min(end_pos.saturating_sub(cur_offset));
let read_bytes = if to_read > 0 { d_file.read_exact_at(&mut chunk_buf[rem_len..rem_len + to_read], cur_offset as u64).unwrap(); cur_offset += to_read; to_read } else { 0 };
let total_available = rem_len + read_bytes; if total_available == 0 { break; } let mut last_nl = total_available; while last_nl > 0 && chunk_buf[last_nl - 1] != b'\n' { last_nl -= 1; }
let (slice, mut p) = (&chunk_buf[..last_nl], 0);
while p < last_nl { let mut doc_id = 0u32; while p < last_nl && slice[p] >= b'0' && slice[p] <= b'9' { doc_id = doc_id * 10 + (slice[p] - b'0') as u32; p += 1; }
while p < last_nl && slice[p] != b'\t' && slice[p] != b'\n' { p += 1; } if p >= last_nl { break; } let text_start = if slice[p] == b'\t' { p + 1 } else { p }; p = text_start;
let text_file_offset = (cur_offset - total_available + text_start) as u64;
while p < last_nl && slice[p] != b'\n' { if slice[p].is_ascii_alphanumeric() { let (start, mut hash, mut count) = (p, 0x517cc1b727220a95u64, 0);
while p < last_nl && slice[p].is_ascii_alphanumeric() { if count < 32 { hash = (hash.rotate_left(5) ^ (slice[p].to_ascii_lowercase() as u64)).wrapping_mul(0x9e3779b97f4a7c15); count += 1; } p += 1; }
let (b1, b2, b3, b4) = ((hash & 0x1FFFFF) as usize, ((hash >> 21) & 0x1FFFFF) as usize, (hash.rotate_left(17) & 0x1FFFFF) as usize, (hash.rotate_left(37) & 0x1FFFFF) as usize);
if ((bloom_ref[b1 >> 6] >> (b1 & 63)) & 1) != 0 && ((bloom_ref[b2 >> 6] >> (b2 & 63)) & 1) != 0 && ((bloom_ref[b3 >> 6] >> (b3 & 63)) & 1) != 0 && ((bloom_ref[b4 >> 6] >> (b4 & 63)) & 1) != 0 {
let mut lower = [0u8; 32]; for k in 0..count { lower[k] = slice[start + k].to_ascii_lowercase(); }
if let Some(&id) = vocab_ref.get(&lower[..count]) { let id_usize = id as usize; if last_seen[id_usize] != doc_id { last_seen[id_usize] = doc_id; local_index[id_usize].push(doc_id); } } } } else { p += 1; } }
text_offsets.push(text_file_offset); text_lens.push((p - text_start) as u32); if p < last_nl && slice[p] == b'\n' { p += 1; } }
rem_len = total_available - last_nl; if rem_len > 0 { chunk_buf.copy_within(last_nl..total_available, 0); } }
WorkerIndexData { index: local_index, doc_text_offsets: text_offsets, doc_text_lens: text_lens } })); }
for h in handles { worker_data.push(h.join().unwrap()); } });
let mut offsets = vec![0u32; vocab_size + 1]; for w in &worker_data { for (tid, list) in w.index.iter().enumerate() { offsets[tid + 1] += list.len() as u32; } } for i in 0..vocab_size { offsets[i + 1] += offsets[i]; }
let mut all_postings = vec![0u32; offsets[vocab_size] as usize]; let mut cur_pos = offsets.clone(); for w in &worker_data { for (tid, list) in w.index.iter().enumerate() { let (s, e) = (cur_pos[tid] as usize, cur_pos[tid] as usize + list.len()); all_postings[s..e].copy_from_slice(list); cur_pos[tid] += list.len() as u32; } }
let (mut doc_text_offsets, mut doc_text_lens) = (Vec::with_capacity(260_000), Vec::with_capacity(260_000)); for w in &worker_data { doc_text_offsets.extend_from_slice(&w.doc_text_offsets); doc_text_lens.extend_from_slice(&w.doc_text_lens); } drop(worker_data);
let num_queries = parsed_queries.len(); let num_threads = thread::available_parallelism().map(|p| p.get()).unwrap_or(12).min(12).max(1);
let chunk_size = (num_queries + num_threads - 1) / num_threads; let mut thread_outputs: Vec<Vec<u8>> = (0..num_threads).map(|_| Vec::new()).collect();
let (offsets_ref, postings_ref) = (&offsets, &all_postings);
thread::scope(|s| { let (queries_ref, docs_file_ref, doc_text_offsets_ref, doc_text_lens_ref) = (&parsed_queries, &docs_file, &doc_text_offsets, &doc_text_lens);
let get_postings = |tid: u32| -> &[u32] { let t = tid as usize; &postings_ref[offsets_ref[t] as usize..offsets_ref[t + 1] as usize] };
for (thread_idx, out_buf) in thread_outputs.iter_mut().enumerate() { let (start, end) = (thread_idx * chunk_size, (thread_idx * chunk_size + chunk_size).min(num_queries)); if start >= end { continue; }
s.spawn(move || { let (mut cur_res, mut next_res, mut scratch, mut doc_buf, mut matches) = (Vec::new(), Vec::new(), Vec::new(), vec![0u8; 8192], Vec::new());
for q_idx in start..end { match &queries_ref[q_idx] {
ParsedQuery::Bool { first_term, ops } => { cur_res.clear(); if let Some(fid) = *first_term { cur_res.extend_from_slice(get_postings(fid)); }
for &(op, opt_tid) in ops { let p: &[u32] = match opt_tid { Some(t) => get_postings(t), None => &[] }; match op { 0 => { intersect_sorted(&cur_res, p, &mut next_res); std::mem::swap(&mut cur_res, &mut next_res); }, 1 => { union_sorted(&cur_res, p, &mut next_res); std::mem::swap(&mut cur_res, &mut next_res); }, 2 => { difference_sorted(&cur_res, p, &mut next_res); std::mem::swap(&mut cur_res, &mut next_res); }, _ => {} } }
fast_write_results(out_buf, &cur_res); }
ParsedQuery::Phrase { term_ids, words } => { if term_ids.is_empty() || term_ids.iter().any(|&t| get_postings(t).is_empty()) { out_buf.extend_from_slice(b"0 0\n"); continue; }
let (num_tids, mut order) = (term_ids.len().min(16), [0usize; 16]); for idx in 0..num_tids { order[idx] = idx; }
order[..num_tids].sort_unstable_by_key(|&idx| get_postings(term_ids[idx]).len()); cur_res.clear(); cur_res.extend_from_slice(get_postings(term_ids[order[0]]));
for &idx in &order[1..num_tids] { if cur_res.is_empty() { break; } intersect_sorted(&cur_res, get_postings(term_ids[idx]), &mut scratch); std::mem::swap(&mut cur_res, &mut scratch); }
let k = words.len(); matches.clear();
for &c_doc in &cur_res { let (f_offset, t_len) = (doc_text_offsets_ref[c_doc as usize], doc_text_lens_ref[c_doc as usize] as usize); if t_len == 0 { continue; }
if t_len > doc_buf.len() { doc_buf.resize(t_len, 0); } if docs_file_ref.read_exact_at(&mut doc_buf[..t_len], f_offset).is_err() { continue; }
let (d_slice, mut dp, mut matched) = (&doc_buf[..t_len], 0, 0);
while dp < t_len { if d_slice[dp].is_ascii_alphanumeric() { let (t_start, mut count) = (dp, 0); while dp < t_len && d_slice[dp].is_ascii_alphanumeric() { count += 1; dp += 1; }
let (mut lower, c) = ([0u8; 32], count.min(32)); for k_idx in 0..c { lower[k_idx] = d_slice[t_start + k_idx].to_ascii_lowercase(); }
let token = &lower[..c];
if token == words[matched].as_slice() { matched += 1; if matched == k { matches.push(c_doc); break; } }
else { matched = if token == words[0].as_slice() { 1 } else { 0 }; } } else { dp += 1; } } }
fast_write_results(out_buf, &matches); }
ParsedQuery::Empty => out_buf.extend_from_slice(b"0 0\n"), } } }); } }); fs::create_dir_all(&out_dir)?; let mut writer = BufWriter::with_capacity(1024 * 1024, File::create(&out_path)?);
for out_buf in thread_outputs { writer.write_all(&out_buf)?; } writer.flush()?; Ok(()) }
