#include <windows.h>
#include <iostream>
#include <fstream>
#include <vector>
#include <string>
#include <string_view>
#include <algorithm>
#include <thread>
#include <cstdint>
#include <cstring>
#include <charconv>

static const uint64_t FNV_OFFSET = 14695981039346656037ULL;
static const uint64_t FNV_PRIME  = 1099511628211ULL;

struct GroupStats {
    uint64_t count = 0;
    uint64_t id_sum = 0;
    int64_t amount_sum = 0;
    int64_t amount_min = INT64_MAX;
    int64_t amount_max = INT64_MIN;
    uint64_t name_bytes = 0;
    uint64_t note_bytes = 0;
    uint32_t names_distinct = 0;
};

static inline void append_uint(std::string& out, uint64_t v) {
    char buf[32];
    auto [ptr, ec] = std::to_chars(buf, buf + sizeof(buf), v);
    out.append(buf, ptr - buf);
}

static inline void append_cents(std::string& out, int64_t cents) {
    if (cents < 0) {
        out.push_back('-');
        cents = -cents;
    }
    append_uint(out, cents / 100);
    out.push_back('.');
    int rem = static_cast<int>(cents % 100);
    if (rem < 10) {
        out.push_back('0');
    }
    append_uint(out, rem);
}

int main(int argc, char* argv[]) {
    auto t0 = std::chrono::high_resolution_clock::now();

    std::string data_dir = argc >= 2 ? argv[1] : "/data";
    std::string out_dir = argc >= 3 ? argv[2] : "/out";

    std::string regions_path = data_dir + "/regions.csv";
    std::string records_path = data_dir + "/records.csv";
    std::string out_path = out_dir + "/aggregates.txt";

    // 1. Read regions.csv
    std::ifstream reg_in(regions_path, std::ios::binary | std::ios::ate);
    if (!reg_in) return 1;
    size_t reg_size = reg_in.tellg();
    reg_in.seekg(0);
    std::vector<char> reg_data(reg_size + 1);
    reg_in.read(reg_data.data(), reg_size);
    reg_data[reg_size] = '\0';
    reg_in.close();

    std::vector<std::string> country_names;
    country_names.reserve(300);

    const size_t MAX_REGIONS = 1000000;
    const uint16_t UNMATCHED_INDEX = 0xFFFF;
    std::vector<uint16_t> region_to_group(MAX_REGIONS, UNMATCHED_INDEX);

    {
        const char* p = reg_data.data();
        const char* end = p + reg_size;
        while (p < end && *p != '\n') p++;
        if (p < end && *p == '\n') p++;

        std::vector<std::pair<std::string, uint32_t>> entries;
        entries.reserve(250000);

        while (p < end) {
            if (*p == '\r' || *p == '\n') { p++; continue; }
            uint32_t r_id = 0;
            while (p < end && *p >= '0' && *p <= '9') {
                r_id = r_id * 10 + (*p - '0');
                p++;
            }
            if (p < end && *p == ',') p++;
            const char* c_start = p;
            while (p < end && *p != '\r' && *p != '\n') p++;
            std::string c_code(c_start, p - c_start);
            if (p < end && *p == '\r') p++;
            if (p < end && *p == '\n') p++;
            if (!c_code.empty()) {
                entries.emplace_back(std::move(c_code), r_id);
            }
        }

        for (const auto& e : entries) {
            country_names.push_back(e.first);
        }
        std::sort(country_names.begin(), country_names.end());
        country_names.erase(std::unique(country_names.begin(), country_names.end()), country_names.end());

        for (const auto& e : entries) {
            auto it = std::lower_bound(country_names.begin(), country_names.end(), e.first);
            uint16_t c_idx = static_cast<uint16_t>(it - country_names.begin());
            if (e.second < MAX_REGIONS) {
                region_to_group[e.second] = c_idx;
            }
        }
    }

    uint16_t num_countries = static_cast<uint16_t>(country_names.size());
    uint16_t unmatched_slot = num_countries;
    country_names.push_back("_UNMATCHED");
    uint16_t num_groups = num_countries + 1;

    // 2. Open records.csv to determine file size and split boundaries
    HANDLE hFile = CreateFileA(records_path.c_str(), GENERIC_READ, FILE_SHARE_READ, NULL, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
    if (hFile == INVALID_HANDLE_VALUE) return 1;
    LARGE_INTEGER fs;
    GetFileSizeEx(hFile, &fs);
    size_t file_size = fs.QuadPart;

    // Find start of data after header
    char header_buf[1024];
    DWORD bytes_read = 0;
    ReadFile(hFile, header_buf, sizeof(header_buf), &bytes_read, NULL);
    size_t data_start = 0;
    if (bytes_read >= 3 && static_cast<unsigned char>(header_buf[0]) == 0xEF &&
        static_cast<unsigned char>(header_buf[1]) == 0xBB &&
        static_cast<unsigned char>(header_buf[2]) == 0xBF) {
        data_start = 3;
    }
    while (data_start < bytes_read && header_buf[data_start] != '\n') data_start++;
    if (data_start < bytes_read && header_buf[data_start] == '\n') data_start++;

    unsigned int nthreads = std::max(1u, std::thread::hardware_concurrency());
    std::vector<size_t> splits(nthreads + 1);
    splits[0] = data_start;
    splits[nthreads] = file_size;

    for (unsigned int tid = 1; tid < nthreads; ++tid) {
        size_t target = data_start + (tid * (file_size - data_start) / nthreads);
        OVERLAPPED ov = {0};
        ov.Offset = static_cast<DWORD>(target);
        ov.OffsetHigh = static_cast<DWORD>(target >> 32);
        char scan_buf[2048];
        DWORD nr = 0;
        ReadFile(hFile, scan_buf, sizeof(scan_buf), &nr, &ov);

        // Find .dd\r?\n\d+, pattern
        size_t found_pos = target;
        for (DWORD i = 0; i + 6 < nr; ++i) {
            if (scan_buf[i] == '.' &&
                scan_buf[i+1] >= '0' && scan_buf[i+1] <= '9' &&
                scan_buf[i+2] >= '0' && scan_buf[i+2] <= '9') {
                DWORD j = i + 3;
                if (j < nr && scan_buf[j] == '\r') j++;
                if (j < nr && scan_buf[j] == '\n') {
                    j++;
                    // verify digits followed by comma
                    DWORD k = j;
                    while (k < nr && scan_buf[k] >= '0' && scan_buf[k] <= '9') k++;
                    if (k > j && k < nr && scan_buf[k] == ',') {
                        found_pos = target + j;
                        break;
                    }
                }
            }
        }
        splits[tid] = found_pos;
    }
    CloseHandle(hFile);

    std::vector<std::vector<GroupStats>> thread_stats(nthreads, std::vector<GroupStats>(num_groups));
    std::vector<std::vector<std::vector<uint64_t>>> thread_names(nthreads, std::vector<std::vector<uint64_t>>(num_groups));
    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        for (uint16_t g = 0; g < num_groups; ++g) {
            thread_names[tid][g].reserve(1500);
        }
    }

    std::vector<std::thread> workers;
    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        workers.emplace_back([&, tid]() {
            HANDLE thFile = CreateFileA(records_path.c_str(), GENERIC_READ, FILE_SHARE_READ, NULL, OPEN_EXISTING, FILE_FLAG_SEQUENTIAL_SCAN, NULL);
            if (thFile == INVALID_HANDLE_VALUE) return;

            size_t cur_file_off = splits[tid];
            size_t end_file_off = splits[tid + 1];

            const size_t CHUNK_SIZE = 512 * 1024;
            std::vector<char> raw_buf(CHUNK_SIZE + 4096);
            char* buf = raw_buf.data();
            size_t leftover = 0;

            auto& stats = thread_stats[tid];
            auto& names = thread_names[tid];

            while (cur_file_off < end_file_off || leftover > 0) {
                size_t to_read = 0;
                if (cur_file_off < end_file_off) {
                    to_read = std::min(CHUNK_SIZE, end_file_off - cur_file_off);
                }

                DWORD bytes_read = 0;
                if (to_read > 0) {
                    OVERLAPPED ov = {0};
                    ov.Offset = static_cast<DWORD>(cur_file_off);
                    ov.OffsetHigh = static_cast<DWORD>(cur_file_off >> 32);
                    ReadFile(thFile, buf + leftover, static_cast<DWORD>(to_read), &bytes_read, &ov);
                    cur_file_off += bytes_read;
                }

                size_t total_buf = leftover + bytes_read;
                if (total_buf == 0) break;

                const char* p = buf;
                const char* p_end = buf + total_buf;
                bool is_last_chunk = (cur_file_off >= end_file_off);

                while (p < p_end) {
                    // Check if we have enough buffer for a full record
                    if (!is_last_chunk && (p_end - p) < 512) {
                        break;
                    }

                    if (p + 2 < p_end &&
                        static_cast<unsigned char>(p[0]) == 0xEF &&
                        static_cast<unsigned char>(p[1]) == 0xBB &&
                        static_cast<unsigned char>(p[2]) == 0xBF) {
                        p += 3;
                    }

                    uint64_t id = 0;
                    while (p < p_end && *p >= '0' && *p <= '9') {
                        id = id * 10 + (*p - '0');
                        p++;
                    }
                    if (p < p_end && *p == ',') p++;

                    uint64_t name_hash = FNV_OFFSET;
                    uint64_t name_len = 0;
                    if (p < p_end && *p == '"') {
                        p++;
                        while (p < p_end) {
                            if (*p == '"') {
                                if (p + 1 < p_end && *(p + 1) == '"') {
                                    name_hash = (name_hash ^ static_cast<unsigned char>('"')) * FNV_PRIME;
                                    name_len++;
                                    p += 2;
                                } else {
                                    p++;
                                    break;
                                }
                            } else {
                                name_hash = (name_hash ^ static_cast<unsigned char>(*p)) * FNV_PRIME;
                                name_len++;
                                p++;
                            }
                        }
                    } else {
                        while (p < p_end && *p != ',') {
                            name_hash = (name_hash ^ static_cast<unsigned char>(*p)) * FNV_PRIME;
                            name_len++;
                            p++;
                        }
                    }
                    if (p < p_end && *p == ',') p++;

                    uint64_t note_len = 0;
                    if (p < p_end && *p == '"') {
                        p++;
                        while (p < p_end) {
                            if (*p == '"') {
                                if (p + 1 < p_end && *(p + 1) == '"') {
                                    note_len++;
                                    p += 2;
                                } else {
                                    p++;
                                    break;
                                }
                            } else {
                                note_len++;
                                p++;
                            }
                        }
                    } else {
                        while (p < p_end && *p != ',') {
                            note_len++;
                            p++;
                        }
                    }
                    if (p < p_end && *p == ',') p++;

                    uint32_t region_id = 0;
                    while (p < p_end && *p >= '0' && *p <= '9') {
                        region_id = region_id * 10 + (*p - '0');
                        p++;
                    }
                    if (p < p_end && *p == ',') p++;

                    bool neg = false;
                    if (p < p_end && *p == '-') {
                        neg = true;
                        p++;
                    }
                    int64_t dollars = 0;
                    while (p < p_end && *p >= '0' && *p <= '9') {
                        dollars = dollars * 10 + (*p - '0');
                        p++;
                    }
                    int cents_val = 0;
                    if (p < p_end && *p == '.') {
                        p++;
                        if (p < p_end && *p >= '0' && *p <= '9') {
                            cents_val = (*p - '0') * 10;
                            p++;
                            if (p < p_end && *p >= '0' && *p <= '9') {
                                cents_val += (*p - '0');
                                p++;
                            }
                        }
                    }
                    int64_t total_cents = dollars * 100 + cents_val;
                    if (neg) total_cents = -total_cents;

                    if (p < p_end && *p == '\r') p++;
                    if (p < p_end && *p == '\n') p++;

                    uint16_t g_idx = unmatched_slot;
                    if (region_id < MAX_REGIONS) {
                        uint16_t found = region_to_group[region_id];
                        if (found != UNMATCHED_INDEX) g_idx = found;
                    }

                    auto& st = stats[g_idx];
                    st.count++;
                    st.id_sum += id;
                    st.amount_sum += total_cents;
                    if (total_cents < st.amount_min) st.amount_min = total_cents;
                    if (total_cents > st.amount_max) st.amount_max = total_cents;
                    st.name_bytes += name_len;
                    st.note_bytes += note_len;

                    names[g_idx].push_back(name_hash);
                }

                leftover = p_end - p;
                if (leftover > 0) {
                    std::memmove(buf, p, leftover);
                }
                if (bytes_read == 0 && leftover > 0 && is_last_chunk) {
                    break;
                }
            }

            CloseHandle(thFile);
        });
    }

    for (auto& w : workers) w.join();

    std::vector<GroupStats> final_aggs(num_groups);
    for (uint16_t g = 0; g < num_groups; ++g) {
        auto& dst = final_aggs[g];
        for (unsigned int tid = 0; tid < nthreads; ++tid) {
            const auto& src = thread_stats[tid][g];
            if (src.count == 0) continue;
            dst.count += src.count;
            dst.id_sum += src.id_sum;
            dst.amount_sum += src.amount_sum;
            if (src.amount_min < dst.amount_min) dst.amount_min = src.amount_min;
            if (src.amount_max > dst.amount_max) dst.amount_max = src.amount_max;
            dst.name_bytes += src.name_bytes;
            dst.note_bytes += src.note_bytes;
        }
    }

    // Parallel sort distinct names for each group
    std::vector<std::thread> sorters;
    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        sorters.emplace_back([&, tid]() {
            size_t g_start = tid * num_groups / nthreads;
            size_t g_end = (tid + 1) * num_groups / nthreads;
            std::vector<uint64_t> group_pool;
            group_pool.reserve(16384);

            for (size_t g = g_start; g < g_end; ++g) {
                group_pool.clear();
                for (unsigned int t = 0; t < nthreads; ++t) {
                    const auto& v = thread_names[t][g];
                    group_pool.insert(group_pool.end(), v.begin(), v.end());
                }
                if (!group_pool.empty()) {
                    std::sort(group_pool.begin(), group_pool.end());
                    auto end_it = std::unique(group_pool.begin(), group_pool.end());
                    final_aggs[g].names_distinct = static_cast<uint32_t>(end_it - group_pool.begin());
                }
            }
        });
    }
    for (auto& s : sorters) s.join();

    // Write output
    std::string out_buf;
    out_buf.reserve(64 * 1024);

    for (uint16_t g = 0; g < num_groups; ++g) {
        const auto& agg = final_aggs[g];
        if (agg.count == 0) continue;
        out_buf.append(country_names[g]);
        out_buf.push_back(' ');
        append_uint(out_buf, agg.count);
        out_buf.push_back(' ');
        append_uint(out_buf, agg.id_sum);
        out_buf.push_back(' ');
        append_cents(out_buf, agg.amount_sum);
        out_buf.push_back(' ');
        append_cents(out_buf, agg.amount_min);
        out_buf.push_back(' ');
        append_cents(out_buf, agg.amount_max);
        out_buf.push_back(' ');
        append_uint(out_buf, agg.names_distinct);
        out_buf.push_back(' ');
        append_uint(out_buf, agg.name_bytes);
        out_buf.push_back(' ');
        append_uint(out_buf, agg.note_bytes);
        out_buf.push_back('\n');
    }

    std::ofstream out(out_path, std::ios::binary);
    out.write(out_buf.data(), out_buf.size());
    out.close();

    return 0;
}
