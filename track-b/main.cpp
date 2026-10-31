#ifdef _WIN32
#include <windows.h>
#else
#include <sys/mman.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#endif

#include <iostream>
#include <fstream>
#include <vector>
#include <string>
#include <string_view>
#include <algorithm>
#include <thread>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <charconv>

struct FileView {
    const char* data = nullptr;
    size_t size = 0;
#ifdef _WIN32
    HANDLE hFile = INVALID_HANDLE_VALUE;
    HANDLE hMap = NULL;
#else
    int fd = -1;
#endif

    bool open(const std::string& path) {
#ifdef _WIN32
        hFile = CreateFileA(path.c_str(), GENERIC_READ, FILE_SHARE_READ, NULL, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
        if (hFile == INVALID_HANDLE_VALUE) return false;
        LARGE_INTEGER fs;
        GetFileSizeEx(hFile, &fs);
        size = fs.QuadPart;
        hMap = CreateFileMappingA(hFile, NULL, PAGE_READONLY, 0, 0, NULL);
        if (!hMap) { CloseHandle(hFile); return false; }
        data = static_cast<const char*>(MapViewOfFile(hMap, FILE_MAP_READ, 0, 0, 0));
        return data != nullptr;
#else
        fd = ::open(path.c_str(), O_RDONLY);
        if (fd < 0) return false;
        struct stat st;
        if (fstat(fd, &st) < 0) { ::close(fd); return false; }
        size = st.st_size;
        data = static_cast<const char*>(mmap(nullptr, size, PROT_READ, MAP_SHARED, fd, 0));
        return data != MAP_FAILED;
#endif
    }

    void close() {
#ifdef _WIN32
        if (data) UnmapViewOfFile(data);
        if (hMap) CloseHandle(hMap);
        if (hFile != INVALID_HANDLE_VALUE) CloseHandle(hFile);
#else
        if (data && data != MAP_FAILED) munmap(const_cast<char*>(data), size);
        if (fd >= 0) ::close(fd);
#endif
        data = nullptr;
    }
    ~FileView() { close(); }
};

static inline bool is_token_char(char c) {
    return (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z');
}

static inline char to_lower_ascii(char c) {
    if (c >= 'A' && c <= 'Z') return static_cast<char>(c + 32);
    return c;
}

static inline void append_uint64(std::string& out, uint64_t v) {
    char buf[32];
    auto [ptr, ec] = std::to_chars(buf, buf + sizeof(buf), v);
    out.append(buf, ptr - buf);
}

static void set_and_into(const uint32_t* a, size_t a_len, const uint32_t* b, size_t b_len, std::vector<uint32_t>& out) {
    out.clear();
    if (a_len == 0 || b_len == 0) return;
    if (a_len > 16 * b_len) {
        size_t last_i = 0;
        for (size_t j = 0; j < b_len; ++j) {
            uint32_t val = b[j];
            const uint32_t* it = std::lower_bound(a + last_i, a + a_len, val);
            if (it == a + a_len) break;
            if (*it == val) {
                out.push_back(val);
                last_i = (it - a) + 1;
            } else {
                last_i = it - a;
            }
        }
        return;
    }
    if (b_len > 16 * a_len) {
        size_t last_j = 0;
        for (size_t i = 0; i < a_len; ++i) {
            uint32_t val = a[i];
            const uint32_t* it = std::lower_bound(b + last_j, b + b_len, val);
            if (it == b + b_len) break;
            if (*it == val) {
                out.push_back(val);
                last_j = (it - b) + 1;
            } else {
                last_j = it - b;
            }
        }
        return;
    }
    size_t i = 0, j = 0;
    while (i < a_len && j < b_len) {
        if (a[i] == b[j]) {
            out.push_back(a[i]);
            i++; j++;
        } else if (a[i] < b[j]) {
            i++;
        } else {
            j++;
        }
    }
}

static void set_or_into(const uint32_t* a, size_t a_len, const uint32_t* b, size_t b_len, std::vector<uint32_t>& out) {
    out.clear();
    size_t i = 0, j = 0;
    while (i < a_len && j < b_len) {
        if (a[i] == b[j]) {
            out.push_back(a[i]);
            i++; j++;
        } else if (a[i] < b[j]) {
            out.push_back(a[i]);
            i++;
        } else {
            out.push_back(b[j]);
            j++;
        }
    }
    while (i < a_len) out.push_back(a[i++]);
    while (j < b_len) out.push_back(b[j++]);
}

static void set_diff_into(const uint32_t* a, size_t a_len, const uint32_t* b, size_t b_len, std::vector<uint32_t>& out) {
    out.clear();
    if (a_len == 0) return;
    if (b_len == 0) {
        out.assign(a, a + a_len);
        return;
    }
    if (b_len > 16 * a_len) {
        size_t last_j = 0;
        for (size_t i = 0; i < a_len; ++i) {
            uint32_t val = a[i];
            const uint32_t* it = std::lower_bound(b + last_j, b + b_len, val);
            if (it == b + b_len) {
                out.insert(out.end(), a + i, a + a_len);
                break;
            }
            if (*it != val) {
                out.push_back(val);
                last_j = it - b;
            } else {
                last_j = (it - b) + 1;
            }
        }
        return;
    }
    size_t i = 0, j = 0;
    while (i < a_len && j < b_len) {
        if (a[i] == b[j]) {
            i++; j++;
        } else if (a[i] < b[j]) {
            out.push_back(a[i]);
            i++;
        } else {
            j++;
        }
    }
    while (i < a_len) out.push_back(a[i++]);
}

static inline uint64_t hash_token_sv(std::string_view sv) {
    uint64_t h = 14695981039346656037ULL;
    for (char c : sv) {
        h ^= static_cast<unsigned char>(to_lower_ascii(c));
        h *= 1099511628211ULL;
    }
    return (h == 0) ? 1 : h;
}

int main(int argc, char* argv[]) {
    auto t0 = std::chrono::high_resolution_clock::now();
    std::ios_base::sync_with_stdio(false);
    std::cin.tie(nullptr);

    std::string data_dir = argc >= 2 ? argv[1] : "/data";
    std::string out_dir = argc >= 3 ? argv[2] : "/out";

    std::string docs_path = data_dir + "/docs.txt";
    std::string queries_path = data_dir + "/queries.txt";
    std::string out_path = out_dir + "/results.txt";

    // 1. Read all queries FIRST to build exact query vocabulary
    FileView qview;
    if (!qview.open(queries_path)) return 1;

    std::vector<std::string_view> query_lines;
    query_lines.reserve(50000);
    const char* qp = qview.data;
    const char* qend = qp + qview.size;
    while (qp < qend) {
        const char* lstart = qp;
        while (qp < qend && *qp != '\n') qp++;
        const char* lend = qp;
        if (qp < qend && *qp == '\n') qp++;
        if (lend > lstart && *(lend - 1) == '\r') lend--;
        if (lend > lstart) {
            query_lines.emplace_back(lstart, lend - lstart);
        }
    }

    // Hash table for query vocabulary only (~75,000 terms)
    const size_t HASH_CAP = 262144;
    const size_t HASH_MASK = HASH_CAP - 1;
    std::vector<uint64_t> vocab_hash(HASH_CAP, 0);
    std::vector<uint32_t> vocab_id(HASH_CAP, UINT32_MAX);
    uint32_t num_terms = 0;

    auto get_or_add_query_term = [&](uint64_t h) -> uint32_t {
        size_t slot = h & HASH_MASK;
        while (true) {
            if (vocab_hash[slot] == 0) {
                vocab_hash[slot] = h;
                uint32_t id = num_terms++;
                vocab_id[slot] = id;
                return id;
            }
            if (vocab_hash[slot] == h) return vocab_id[slot];
            slot = (slot + 1) & HASH_MASK;
        }
    };

    auto lookup_term_id = [&](uint64_t h) -> int32_t {
        size_t slot = h & HASH_MASK;
        while (true) {
            if (vocab_hash[slot] == 0) return -1;
            if (vocab_hash[slot] == h) return static_cast<int32_t>(vocab_id[slot]);
            slot = (slot + 1) & HASH_MASK;
        }
    };

    for (std::string_view q : query_lines) {
        if (q.starts_with("BOOL ")) {
            size_t pos = 5;
            while (pos < q.size()) {
                size_t sp = q.find(' ', pos);
                std::string_view w = (sp == std::string_view::npos) ? q.substr(pos) : q.substr(pos, sp - pos);
                get_or_add_query_term(hash_token_sv(w));
                if (sp == std::string_view::npos) break;
                pos = q.find(' ', sp + 1);
                if (pos == std::string_view::npos) break;
                pos++;
            }
        } else if (q.starts_with("PHRASE \"") && q.ends_with('"')) {
            std::string_view inner = q.substr(8, q.size() - 9);
            size_t pos = 0;
            while (pos < inner.size()) {
                size_t sp = inner.find(' ', pos);
                std::string_view w = (sp == std::string_view::npos) ? inner.substr(pos) : inner.substr(pos, sp - pos);
                get_or_add_query_term(hash_token_sv(w));
                if (sp == std::string_view::npos) break;
                pos = sp + 1;
            }
        }
    }
    auto t_queries = std::chrono::high_resolution_clock::now();

    // 2. Map docs.txt with zero copy
    FileView dview;
    if (!dview.open(docs_path)) return 1;

    struct DocSpan {
        uint32_t offset;
        uint32_t length;
    };
    std::vector<DocSpan> docs(250000);

    unsigned int nthreads = std::max(1u, std::thread::hardware_concurrency());
    std::vector<const char*> chunk_starts(nthreads);
    std::vector<const char*> chunk_ends(nthreads);
    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        size_t off = tid * dview.size / nthreads;
        const char* p = dview.data + off;
        if (tid > 0) {
            while (p < dview.data + dview.size && *p != '\n') p++;
            if (p < dview.data + dview.size) p++;
        }
        chunk_starts[tid] = p;
    }
    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        chunk_ends[tid] = (tid + 1 < nthreads) ? chunk_starts[tid + 1] : (dview.data + dview.size);
    }

    struct PostingItem {
        uint32_t term_id;
        uint32_t doc_id;
    };
    std::vector<std::vector<PostingItem>> thread_postings(nthreads);
    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        thread_postings[tid].reserve(1500000);
    }

    // Single-pass tokenization and indexing across all threads
    std::vector<std::thread> parse_workers;
    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        parse_workers.emplace_back([&, tid]() {
            const char* p = chunk_starts[tid];
            const char* end = chunk_ends[tid];
            auto& postings = thread_postings[tid];
            std::vector<uint32_t> cur_doc_terms;
            cur_doc_terms.reserve(256);

            while (p < end) {
                const char* line_start = p;
                while (p < end && *p != '\t' && *p != '\n') p++;
                if (p >= end || *p == '\n') {
                    if (p < end && *p == '\n') p++;
                    continue;
                }
                uint32_t doc_id = 0;
                std::from_chars(line_start, p, doc_id);
                p++;
                const char* text_start = p;
                while (p < end && *p != '\n') p++;
                const char* text_end = p;
                if (p < end && *p == '\n') p++;
                if (text_end > text_start && *(text_end - 1) == '\r') text_end--;

                if (doc_id < docs.size()) {
                    docs[doc_id] = DocSpan{static_cast<uint32_t>(text_start - dview.data), static_cast<uint32_t>(text_end - text_start)};
                }

                cur_doc_terms.clear();
                const char* tp = text_start;
                while (tp < text_end) {
                    if (is_token_char(*tp)) {
                        uint64_t h = 14695981039346656037ULL;
                        while (tp < text_end && is_token_char(*tp)) {
                            h ^= static_cast<unsigned char>(to_lower_ascii(*tp));
                            h *= 1099511628211ULL;
                            tp++;
                        }
                        if (h == 0) h = 1;
                        int32_t term_id = lookup_term_id(h);
                        if (term_id >= 0) cur_doc_terms.push_back(static_cast<uint32_t>(term_id));
                    } else {
                        tp++;
                    }
                }
                if (!cur_doc_terms.empty()) {
                    std::sort(cur_doc_terms.begin(), cur_doc_terms.end());
                    cur_doc_terms.erase(std::unique(cur_doc_terms.begin(), cur_doc_terms.end()), cur_doc_terms.end());
                    for (uint32_t tid_val : cur_doc_terms) {
                        postings.push_back({tid_val, doc_id});
                    }
                }
            }
        });
    }
    for (auto& w : parse_workers) w.join();

    // Count terms across thread postings in parallel
    std::vector<std::vector<uint32_t>> thread_counts(nthreads, std::vector<uint32_t>(num_terms, 0));
    std::vector<std::thread> count_threads;
    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        count_threads.emplace_back([&, tid]() {
            auto& tc = thread_counts[tid];
            for (const auto& item : thread_postings[tid]) {
                tc[item.term_id]++;
            }
        });
    }
    for (auto& t : count_threads) t.join();

    std::vector<uint32_t> postings_head(num_terms + 1, 0);
    std::vector<std::vector<uint32_t>> thread_offsets(nthreads, std::vector<uint32_t>(num_terms, 0));

    uint32_t total_postings = 0;
    for (uint32_t term_id = 0; term_id < num_terms; ++term_id) {
        postings_head[term_id] = total_postings;
        for (unsigned int tid = 0; tid < nthreads; ++tid) {
            thread_offsets[tid][term_id] = total_postings;
            total_postings += thread_counts[tid][term_id];
        }
    }
    postings_head[num_terms] = total_postings;
    thread_counts.clear(); thread_counts.shrink_to_fit();

    std::vector<uint32_t> all_postings(total_postings);

    // Parallel scatter postings
    std::vector<std::thread> scatter_threads;
    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        scatter_threads.emplace_back([&, tid]() {
            auto& offsets = thread_offsets[tid];
            for (const auto& item : thread_postings[tid]) {
                all_postings[offsets[item.term_id]++] = item.doc_id;
            }
            thread_postings[tid].clear();
            thread_postings[tid].shrink_to_fit();
        });
    }
    for (auto& t : scatter_threads) t.join();
    thread_offsets.clear(); thread_offsets.shrink_to_fit();

    auto t_scatter = std::chrono::high_resolution_clock::now();

    // 3. Parallel query processing across all cores
    size_t num_q = query_lines.size();
    std::vector<std::string> thread_outputs(nthreads);
    std::vector<std::thread> workers;

    const char* docs_raw = dview.data;

    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        workers.emplace_back([&, tid]() {
            size_t start = tid * num_q / nthreads;
            size_t end = (tid + 1) * num_q / nthreads;

            std::string& out = thread_outputs[tid];
            out.reserve((end - start) * 64);

            std::vector<uint32_t> buf_a;
            std::vector<uint32_t> buf_b;
            buf_a.reserve(8192);
            buf_b.reserve(8192);

            std::vector<uint32_t> phrase_tokens;
            phrase_tokens.reserve(32);

            std::vector<std::string_view> tokens;
            tokens.reserve(16);

            std::vector<int32_t> term_ids;
            term_ids.reserve(16);

            auto format_res = [&](const uint32_t* ids, size_t count) {
                append_uint64(out, count);
                out.push_back(' ');
                uint64_t sum_val = 0;
                for (size_t i = 0; i < count; ++i) {
                    sum_val += ids[i];
                }
                append_uint64(out, sum_val);
                size_t top_k = std::min(count, (size_t)10);
                for (size_t i = 0; i < top_k; ++i) {
                    out.push_back(' ');
                    append_uint64(out, ids[i]);
                }
                out.push_back('\n');
            };

            for (size_t qi = start; qi < end; ++qi) {
                std::string_view q = query_lines[qi];

                if (q.starts_with("BOOL ")) {
                    tokens.clear();
                    size_t pos = 5;
                    while (pos < q.size()) {
                        size_t next_space = q.find(' ', pos);
                        if (next_space == std::string_view::npos) {
                            tokens.push_back(q.substr(pos));
                            break;
                        } else {
                            tokens.push_back(q.substr(pos, next_space - pos));
                            pos = next_space + 1;
                        }
                    }

                    if (tokens.empty()) {
                        format_res(nullptr, 0);
                        continue;
                    }

                    int32_t t0 = lookup_term_id(hash_token_sv(tokens[0]));
                    const uint32_t* p0_data = nullptr;
                    size_t p0_len = 0;
                    if (t0 >= 0 && static_cast<size_t>(t0) < num_terms) {
                        p0_data = all_postings.data() + postings_head[t0];
                        p0_len = postings_head[t0 + 1] - postings_head[t0];
                    }
                    buf_a.assign(p0_data, p0_data + p0_len);

                    bool using_a = true;

                    for (size_t i = 1; i + 1 < tokens.size(); i += 2) {
                        std::string_view op = tokens[i];
                        int32_t tid_val = lookup_term_id(hash_token_sv(tokens[i + 1]));
                        const uint32_t* next_p_data = nullptr;
                        size_t next_p_len = 0;
                        if (tid_val >= 0 && static_cast<size_t>(tid_val) < num_terms) {
                            next_p_data = all_postings.data() + postings_head[tid_val];
                            next_p_len = postings_head[tid_val + 1] - postings_head[tid_val];
                        }

                        const uint32_t* cur_data = using_a ? buf_a.data() : buf_b.data();
                        size_t cur_len = using_a ? buf_a.size() : buf_b.size();
                        std::vector<uint32_t>& target = using_a ? buf_b : buf_a;

                        if (op == "AND") {
                            set_and_into(cur_data, cur_len, next_p_data, next_p_len, target);
                        } else if (op == "OR") {
                            set_or_into(cur_data, cur_len, next_p_data, next_p_len, target);
                        } else if (op == "NOT") {
                            set_diff_into(cur_data, cur_len, next_p_data, next_p_len, target);
                        }
                        using_a = !using_a;
                    }

                    const uint32_t* res_data = using_a ? buf_a.data() : buf_b.data();
                    size_t res_len = using_a ? buf_a.size() : buf_b.size();
                    format_res(res_data, res_len);

                } else if (q.starts_with("PHRASE \"") && q.ends_with('"')) {
                    std::string_view inner = q.substr(8, q.size() - 9);
                    term_ids.clear();
                    bool missing = false;

                    size_t pos = 0;
                    while (pos < inner.size()) {
                        size_t sp = inner.find(' ', pos);
                        std::string_view w = (sp == std::string_view::npos) ? inner.substr(pos) : inner.substr(pos, sp - pos);
                        pos = (sp == std::string_view::npos) ? inner.size() : sp + 1;

                        int32_t tid_val = lookup_term_id(hash_token_sv(w));
                        if (tid_val < 0 || static_cast<size_t>(tid_val) >= num_terms) {
                            missing = true;
                            break;
                        }
                        term_ids.push_back(tid_val);
                    }

                    if (missing || term_ids.empty()) {
                        format_res(nullptr, 0);
                        continue;
                    }

                    const uint32_t* t0_data = all_postings.data() + postings_head[term_ids[0]];
                    size_t t0_len = postings_head[term_ids[0] + 1] - postings_head[term_ids[0]];
                    buf_a.assign(t0_data, t0_data + t0_len);
                    bool using_a = true;

                    for (size_t i = 1; i < term_ids.size(); ++i) {
                        const uint32_t* next_p_data = all_postings.data() + postings_head[term_ids[i]];
                        size_t next_p_len = postings_head[term_ids[i] + 1] - postings_head[term_ids[i]];

                        const uint32_t* cur_data = using_a ? buf_a.data() : buf_b.data();
                        size_t cur_len = using_a ? buf_a.size() : buf_b.size();
                        std::vector<uint32_t>& target = using_a ? buf_b : buf_a;
                        set_and_into(cur_data, cur_len, next_p_data, next_p_len, target);
                        using_a = !using_a;
                    }

                    const uint32_t* cand_data = using_a ? buf_a.data() : buf_b.data();
                    size_t cand_len = using_a ? buf_a.size() : buf_b.size();

                    std::vector<uint32_t>& matches = using_a ? buf_b : buf_a;
                    matches.clear();
                    size_t k_terms = term_ids.size();

                    for (size_t ci = 0; ci < cand_len; ++ci) {
                        uint32_t d_id = cand_data[ci];
                        const auto& span = docs[d_id];
                        const char* dp = docs_raw + span.offset;
                        const char* dend = dp + span.length;

                        phrase_tokens.clear();
                        while (dp < dend) {
                            if (is_token_char(*dp)) {
                                uint64_t h = 14695981039346656037ULL;
                                while (dp < dend && is_token_char(*dp)) {
                                    h ^= static_cast<unsigned char>(to_lower_ascii(*dp));
                                    h *= 1099511628211ULL;
                                    dp++;
                                }
                                if (h == 0) h = 1;
                                int32_t tok_id = lookup_term_id(h);
                                phrase_tokens.push_back(tok_id >= 0 ? static_cast<uint32_t>(tok_id) : UINT32_MAX);
                            } else {
                                dp++;
                            }
                        }

                        if (phrase_tokens.size() < k_terms) continue;

                        bool found = false;
                        for (size_t ti = 0; ti + k_terms <= phrase_tokens.size(); ++ti) {
                            bool ok = true;
                            for (size_t ki = 0; ki < k_terms; ++ki) {
                                if (phrase_tokens[ti + ki] != static_cast<uint32_t>(term_ids[ki])) {
                                    ok = false;
                                    break;
                                }
                            }
                            if (ok) {
                                found = true;
                                break;
                            }
                        }
                        if (found) {
                            matches.push_back(d_id);
                        }
                    }
                    format_res(matches.data(), matches.size());
                }
            }
        });
    }
    for (auto& w : workers) w.join();
    auto t_queries_done = std::chrono::high_resolution_clock::now();

    std::ofstream out(out_path, std::ios::binary);
    for (const auto& s : thread_outputs) {
        out.write(s.data(), s.size());
    }
    out.close();

    auto t_end = std::chrono::high_resolution_clock::now();
    std::cerr << "Queries vocab: " << std::chrono::duration<double>(t_queries - t0).count() * 1000 << " ms\n";
    std::cerr << "Docs index:    " << std::chrono::duration<double>(t_scatter - t_queries).count() * 1000 << " ms\n";
    std::cerr << "Query exec:    " << std::chrono::duration<double>(t_queries_done - t_scatter).count() * 1000 << " ms\n";
    std::cerr << "Output write:  " << std::chrono::duration<double>(t_end - t_queries_done).count() * 1000 << " ms\n";
    std::cerr << "Total time:    " << std::chrono::duration<double>(t_end - t0).count() * 1000 << " ms\n";

    return 0;
}
