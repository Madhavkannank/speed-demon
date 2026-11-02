with open('tools/fast_b_proto.rs') as f:
    code = f.read()

# Replace the inner worker scan loop with AVX2
old_worker_scan = """                let mut p = 0;
                let len = slice.len();
                let mut base_doc = 0u32;
                let mut sp = 0;
                while sp < len && slice[sp] >= b'0' && slice[sp] <= b'9' {
                    base_doc = base_doc * 10 + (slice[sp] - b'0') as u32;
                    sp += 1;
                }
                let mut arena = WorkerArena::new(base_doc, vocab_size, np);
                let mut doc_tokens = Vec::with_capacity(8192);
                let mut last_seen_phrase = vec![u32::MAX; np];

                while p < len {
                    let mut did = 0u32;
                    while p < len && slice[p] >= b'0' && slice[p] <= b'9' {
                        did = did * 10 + (slice[p] - b'0') as u32;
                        p += 1;
                    }
                    let doc_id = did;
                    while p < len && slice[p] != b'\t' && slice[p] != b'\n' {
                        p += 1;
                    }
                    if p < len && slice[p] == b'\t' {
                        p += 1;
                    }

                    doc_tokens.clear();

                    while p < len && slice[p] != b'\n' {
                        if slice[p].is_ascii_alphanumeric() {
                            let start_t = p;
                            while p < len && slice[p].is_ascii_alphanumeric() {
                                p += 1;
                            }
                            let count = p - start_t;
                            let (h, w0, w1, last_b) = hash_token(slice.as_ptr().wrapping_add(start_t), count);
                            let b_idx = (h as usize) & BLOOM_MASK;
                            let mut tid = 0u32;
                            if ((bloom_ref[b_idx >> 6] >> (b_idx & 63)) & 1) != 0 {
                                tid = table_ref.lookup(h, w0, w1, count as u8, last_b);
                            }
                            doc_tokens.push(tid);

                            if tid > 0 {
                                let d_id = dense_map_ref[tid as usize];
                                if d_id != u32::MAX {
                                    let bm_ptr = dense_bitmaps_ptrs_ref[d_id as usize].0;
                                    unsafe {
                                        *bm_ptr.add((doc_id >> 6) as usize) |= 1u64 << (doc_id & 63);
                                    }
                                } else {
                                    arena.add_posting(tid, doc_id);
                                }
                            }

                            // INLINE PHRASE CHECK
                            let dt_len = doc_tokens.len();
                            if dt_len >= 2 && tid > 0 {
                                let prev_tid = doc_tokens[dt_len - 2];
                                if prev_tid > 0 {
                                    let pair_key = ((prev_tid as u64) << 32) | (tid as u64);
                                    let ph_bit = (hash_pair(pair_key) & 0xFFFF) as usize;
                                    if ((pair_bitset_ref[ph_bit >> 6] >> (ph_bit & 63)) & 1) != 0 {
                                        let mut head = phrase_idx_ref.get_head(pair_key);
                                        while head != u32::MAX {
                                            let item = unsafe {
                                                *phrase_idx_ref.items.get_unchecked(head as usize)
                                            };
                                            let plen = item.len as usize;
                                            if dt_len >= plen {
                                                let target_slice = &phrase_idx_ref.terms[item.terms_offset as usize..item.terms_offset as usize + plen];
                                                let candidate_slice = &doc_tokens[dt_len - plen..dt_len];
                                                if candidate_slice == target_slice {
                                                    let ph_idx = item.phrase_id as usize;
                                                    if last_seen_phrase[ph_idx] != doc_id {
                                                        last_seen_phrase[ph_idx] = doc_id;
                                                        arena.phrase_matches[ph_idx].push(doc_id);
                                                    }
                                                }
                                            }
                                            head = item.next;
                                        }
                                    }
                                }
                            }
                        } else {
                            p += 1;
                        }
                    }
                    if p < len && slice[p] == b'\n' {
                        p += 1;
                    }
                }"""

new_worker_scan = """                let mut p = 0;
                let len = slice.len();
                let p_ptr = slice.as_ptr();
                let mut base_doc = 0u32;
                let mut sp = 0;
                while sp < len && *p_ptr.add(sp) >= b'0' && *p_ptr.add(sp) <= b'9' {
                    base_doc = base_doc * 10 + (*p_ptr.add(sp) - b'0') as u32;
                    sp += 1;
                }
                let mut arena = WorkerArena::new(base_doc, vocab_size, np);
                let mut doc_tokens = [0u32; 1024];
                let mut dt_len = 0usize;
                let mut last_seen_phrase = vec![u32::MAX; np];

                let v_20 = _mm256_set1_epi8(0x20);
                let v_a_minus_1 = _mm256_set1_epi8((b'a' - 1) as i8);
                let v_z_plus_1 = _mm256_set1_epi8((b'z' + 1) as i8);
                let v_0_minus_1 = _mm256_set1_epi8((b'0' - 1) as i8);
                let v_9_plus_1 = _mm256_set1_epi8((b'9' + 1) as i8);
                let v_nl = _mm256_set1_epi8(b'\n' as i8);

                while p < len {
                    let mut doc_id = 0u32;
                    while p < len && *p_ptr.add(p) >= b'0' && *p_ptr.add(p) <= b'9' {
                        doc_id = doc_id * 10 + (*p_ptr.add(p) - b'0') as u32;
                        p += 1;
                    }
                    while p < len && *p_ptr.add(p) != b'\t' && *p_ptr.add(p) != b'\n' { p += 1; }
                    if p < len && *p_ptr.add(p) == b'\t' { p += 1; }
                    dt_len = 0;

                    let mut prev_is_alnum = 0u32;
                    let mut doc_ended = false;

                    while p + 32 <= len {
                        let v = _mm256_loadu_si256(p_ptr.add(p) as *const __m256i);
                        let is_nl = _mm256_cmpeq_epi8(v, v_nl);
                        let nl_mask = _mm256_movemask_epi8(is_nl) as u32;

                        let c_or_20 = _mm256_or_si256(v, v_20);
                        let ge_a = _mm256_cmpgt_epi8(c_or_20, v_a_minus_1);
                        let le_z = _mm256_cmpgt_epi8(v_z_plus_1, c_or_20);
                        let is_alpha = _mm256_and_si256(ge_a, le_z);

                        let ge_0 = _mm256_cmpgt_epi8(v, v_0_minus_1);
                        let le_9 = _mm256_cmpgt_epi8(v_9_plus_1, v);
                        let is_digit = _mm256_and_si256(ge_0, le_9);

                        let is_alnum = _mm256_or_si256(is_alpha, is_digit);
                        let mut mask = _mm256_movemask_epi8(is_alnum) as u32;

                        let has_nl = nl_mask != 0;
                        if has_nl {
                            let nl_off = nl_mask.trailing_zeros();
                            mask &= (1u32 << nl_off).wrapping_sub(1);
                        }

                        let mut start_bits = mask & !((mask << 1) | prev_is_alnum);
                        prev_is_alnum = (mask >> 31) & 1;

                        let mut spanned = false;
                        while start_bits != 0 {
                            let off = start_bits.trailing_zeros() as usize;
                            start_bits &= start_bits - 1;
                            let tok_start = p + off;
                            let not_mask = (!mask) >> off;
                            let tok_len = if not_mask != 0 {
                                not_mask.trailing_zeros() as usize
                            } else {
                                let mut l = 32 - off;
                                while p + off + l < len && (*p_ptr.add(p + off + l)).is_ascii_alphanumeric() {
                                    l += 1;
                                }
                                spanned = true;
                                l
                            };

                            let (h, w0, w1, last_b) = hash_token(p_ptr.add(tok_start), tok_len);
                            let b_idx = (h as usize) & BLOOM_MASK;
                            let mut tid = 0u32;
                            if ((bloom_ref[b_idx >> 6] >> (b_idx & 63)) & 1) != 0 {
                                tid = table_ref.lookup(h, w0, w1, tok_len as u8, last_b);
                            }
                            if dt_len < 1024 {
                                doc_tokens[dt_len] = tid;
                                dt_len += 1;
                            }

                            if tid > 0 {
                                let d_id = dense_map_ref[tid as usize];
                                if d_id != u32::MAX {
                                    let bm_ptr = dense_bitmaps_ptrs_ref[d_id as usize].0;
                                    unsafe {
                                        *bm_ptr.add((doc_id >> 6) as usize) |= 1u64 << (doc_id & 63);
                                    }
                                } else {
                                    arena.add_posting(tid, doc_id);
                                }
                            }

                            // INLINE PHRASE CHECK
                            if dt_len >= 2 && tid > 0 {
                                let prev_tid = doc_tokens[dt_len - 2];
                                if prev_tid > 0 {
                                    let pair_key = ((prev_tid as u64) << 32) | (tid as u64);
                                    let ph_bit = (hash_pair(pair_key) & 0xFFFF) as usize;
                                    if ((pair_bitset_ref[ph_bit >> 6] >> (ph_bit & 63)) & 1) != 0 {
                                        let mut head = phrase_idx_ref.get_head(pair_key);
                                        while head != u32::MAX {
                                            let item = unsafe {
                                                *phrase_idx_ref.items.get_unchecked(head as usize)
                                            };
                                            let plen = item.len as usize;
                                            if dt_len >= plen {
                                                let target_slice = &phrase_idx_ref.terms[item.terms_offset as usize..item.terms_offset as usize + plen];
                                                let candidate_slice = &doc_tokens[dt_len - plen..dt_len];
                                                if candidate_slice == target_slice {
                                                    let ph_idx = item.phrase_id as usize;
                                                    if last_seen_phrase[ph_idx] != doc_id {
                                                        last_seen_phrase[ph_idx] = doc_id;
                                                        arena.phrase_matches[ph_idx].push(doc_id);
                                                    }
                                                }
                                            }
                                            head = item.next;
                                        }
                                    }
                                }
                            }

                            if spanned {
                                p = tok_start + tok_len;
                                prev_is_alnum = 0;
                                break;
                            }
                        }

                        if has_nl {
                            let nl_off = nl_mask.trailing_zeros() as usize;
                            p += nl_off + 1;
                            doc_ended = true;
                            break;
                        } else if !spanned {
                            p += 32;
                        }
                    }

                    if !doc_ended {
                        while p < len && *p_ptr.add(p) != b'\n' {
                            if (*p_ptr.add(p)).is_ascii_alphanumeric() {
                                let tok_start = p;
                                while p < len && (*p_ptr.add(p)).is_ascii_alphanumeric() { p += 1; }
                                let tok_len = p - tok_start;
                                let (h, w0, w1, last_b) = hash_token(p_ptr.add(tok_start), tok_len);
                                let b_idx = (h as usize) & BLOOM_MASK;
                                let mut tid = 0u32;
                                if ((bloom_ref[b_idx >> 6] >> (b_idx & 63)) & 1) != 0 {
                                    tid = table_ref.lookup(h, w0, w1, tok_len as u8, last_b);
                                }
                                if dt_len < 1024 {
                                    doc_tokens[dt_len] = tid;
                                    dt_len += 1;
                                }

                                if tid > 0 {
                                    let d_id = dense_map_ref[tid as usize];
                                    if d_id != u32::MAX {
                                        let bm_ptr = dense_bitmaps_ptrs_ref[d_id as usize].0;
                                        unsafe {
                                            *bm_ptr.add((doc_id >> 6) as usize) |= 1u64 << (doc_id & 63);
                                        }
                                    } else {
                                        arena.add_posting(tid, doc_id);
                                    }
                                }

                                if dt_len >= 2 && tid > 0 {
                                    let prev_tid = doc_tokens[dt_len - 2];
                                    if prev_tid > 0 {
                                        let pair_key = ((prev_tid as u64) << 32) | (tid as u64);
                                        let ph_bit = (hash_pair(pair_key) & 0xFFFF) as usize;
                                        if ((pair_bitset_ref[ph_bit >> 6] >> (ph_bit & 63)) & 1) != 0 {
                                            let mut head = phrase_idx_ref.get_head(pair_key);
                                            while head != u32::MAX {
                                                let item = unsafe {
                                                    *phrase_idx_ref.items.get_unchecked(head as usize)
                                                };
                                                let plen = item.len as usize;
                                                if dt_len >= plen {
                                                    let target_slice = &phrase_idx_ref.terms[item.terms_offset as usize..item.terms_offset as usize + plen];
                                                    let candidate_slice = &doc_tokens[dt_len - plen..dt_len];
                                                    if candidate_slice == target_slice {
                                                        let ph_idx = item.phrase_id as usize;
                                                        if last_seen_phrase[ph_idx] != doc_id {
                                                            last_seen_phrase[ph_idx] = doc_id;
                                                            arena.phrase_matches[ph_idx].push(doc_id);
                                                        }
                                                    }
                                                }
                                                head = item.next;
                                            }
                                        }
                                    }
                                }
                            } else {
                                p += 1;
                            }
                        }
                        if p < len && *p_ptr.add(p) == b'\n' { p += 1; }
                    }
                }"""

code = code.replace(old_worker_scan, new_worker_scan)

with open('tools/fast_b_avx2.rs', 'w') as f:
    f.write(code)
print('Generated tools/fast_b_avx2.rs')
