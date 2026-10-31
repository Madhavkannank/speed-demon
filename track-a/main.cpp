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

static const uint64_t FNV_OFFSET = 14695981039346656037ULL;
static const uint64_t FNV_PRIME  = 1099511628211ULL;

static inline uint64_t hash_bytes(const char* p, size_t len) {
    uint64_t h = FNV_OFFSET;
    for (size_t i = 0; i < len; ++i) {
        h = (h ^ static_cast<unsigned char>(p[i])) * FNV_PRIME;
    }
    return h;
}

static inline void append_uint(std::string& out, uint64_t v) {
    char buf[32];
    auto [ptr, ec] = std::to_chars(buf, buf + sizeof(buf), v);
    out.append(buf, ptr - buf);
}

static inline uint32_t parse_hour_id(const char* p) {
    uint32_t day = (p[8] - '0') * 10 + (p[9] - '0');
    uint32_t hr = (p[11] - '0') * 10 + (p[12] - '0');
    uint32_t day_idx = (p[6] == '0') ? (day - 30) : (2 + day - 1);
    return day_idx * 24 + hr - 18;
}

struct LatItem {
    uint32_t slot;
    uint32_t lat;
};

struct EpLookupEntry {
    uint64_t hash = 0;
    const char* ptr = nullptr;
    uint16_t len = 0;
    uint16_t id = 0;
};

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

int main(int argc, char* argv[]) {
    auto t0 = std::chrono::high_resolution_clock::now();
    std::ios_base::sync_with_stdio(false);
    std::cin.tie(nullptr);

    std::string data_dir = argc >= 2 ? argv[1] : "/data";
    std::string out_dir = argc >= 3 ? argv[2] : "/out";

    std::string in_path = data_dir + "/requests.log";
    std::string out_path = out_dir + "/summary.txt";

    FileView view;
    if (!view.open(in_path)) return 1;

    const char* file_start = view.data;
    const char* file_end = view.data + view.size;
    auto t_read = std::chrono::high_resolution_clock::now();

    // Fast endpoint discovery
    enum : size_t { CAP = 4096, MASK = 4095 };
    std::vector<uint64_t> thash(CAP, 0);
    struct Ep { const char* p; uint16_t len; };
    std::vector<Ep> eps;
    eps.reserve(2048);

    const char* p = file_start;
    while (p < file_end && eps.size() < 1842) {
        const char* nl = static_cast<const char*>(std::memchr(p, '\n', file_end - p));
        if (!nl) nl = file_end;
        if (nl - p < 35) {
            p = (nl < file_end && *nl == '\n') ? nl + 1 : nl;
            continue;
        }

        const char* cur = p + 27;
        while (*cur == ' ') cur++;
        while (*cur > ' ') cur++;
        while (*cur == ' ') cur++;

        const char* ep_start = cur;
        while (*cur > ' ') cur++;
        uint16_t ep_len = static_cast<uint16_t>(cur - ep_start);

        uint64_t h = hash_bytes(ep_start, ep_len);
        if (h == 0) h = 1;
        size_t slot = h & MASK;
        bool found = false;
        while (thash[slot] != 0) {
            if (thash[slot] == h) { found = true; break; }
            slot = (slot + 1) & MASK;
        }
        if (!found) {
            thash[slot] = h;
            eps.push_back({ep_start, ep_len});
        }
        p = (nl < file_end && *nl == '\n') ? nl + 1 : nl;
    }

    std::vector<std::string> sorted_eps;
    sorted_eps.reserve(eps.size());
    for (const auto& e : eps) sorted_eps.emplace_back(e.p, e.len);
    std::sort(sorted_eps.begin(), sorted_eps.end());

    uint32_t num_endpoints = static_cast<uint32_t>(sorted_eps.size());
    const uint32_t NUM_HOURS = 72;
    uint32_t num_total_slots = NUM_HOURS * num_endpoints;

    // Lookup table
    enum : size_t { EP_CAP = 8192, EP_MASK = 8191 };
    std::vector<EpLookupEntry> ep_table(EP_CAP);
    for (uint32_t i = 0; i < num_endpoints; ++i) {
        const auto& s = sorted_eps[i];
        uint64_t h = hash_bytes(s.data(), s.size());
        if (h == 0) h = 1;
        size_t slot = h & EP_MASK;
        while (ep_table[slot].hash != 0) {
            slot = (slot + 1) & EP_MASK;
        }
        ep_table[slot] = {h, s.data(), static_cast<uint16_t>(s.size()), static_cast<uint16_t>(i)};
    }

    // Precompute 72 hour strings
    std::vector<std::string> hour_strings(NUM_HOURS);
    {
        int days[4] = {30, 31, 1, 2};
        int months[4] = {10, 10, 11, 11};
        uint32_t idx = 0;
        for (int d = 0; d < 4; ++d) {
            int h_start = (d == 0) ? 18 : 0;
            int h_end = (d == 3) ? 17 : 23;
            for (int h = h_start; h <= h_end; ++h) {
                char buf[32];
                snprintf(buf, sizeof(buf), "2026-%02d-%02dT%02d:00:00Z", months[d], days[d], h);
                hour_strings[idx++] = std::string(buf);
            }
        }
    }
    auto t_ep = std::chrono::high_resolution_clock::now();

    // Chunk splits for workers
    unsigned int nthreads = std::max(1u, std::thread::hardware_concurrency());
    std::vector<const char*> chunk_starts(nthreads + 1);
    chunk_starts[0] = file_start;
    chunk_starts[nthreads] = file_end;
    for (unsigned int tid = 1; tid < nthreads; ++tid) {
        const char* target = file_start + (tid * view.size / nthreads);
        while (target < file_end && *target != '\n') target++;
        if (target < file_end && *target == '\n') target++;
        chunk_starts[tid] = target;
    }

    // Parallel parse
    std::vector<std::vector<uint32_t>> thread_counts(nthreads, std::vector<uint32_t>(num_total_slots, 0));
    std::vector<std::vector<uint32_t>> thread_errors(nthreads, std::vector<uint32_t>(num_total_slots, 0));
    std::vector<std::vector<LatItem>> thread_lats(nthreads);
    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        thread_lats[tid].reserve(400000);
    }

    std::vector<std::thread> workers;
    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        workers.emplace_back([&, tid]() {
            const char* p = chunk_starts[tid];
            const char* p_end = chunk_starts[tid + 1];
            auto& counts = thread_counts[tid];
            auto& errors = thread_errors[tid];
            auto& lats = thread_lats[tid];

            while (p < p_end) {
                if (p_end - p < 35) break;
                uint32_t hour_id = parse_hour_id(p);

                const char* cur = p + 27;
                while (*cur == ' ') cur++;
                while (*cur > ' ') cur++;
                while (*cur == ' ') cur++;

                const char* ep_start = cur;
                while (*cur > ' ') cur++;
                uint16_t ep_len = static_cast<uint16_t>(cur - ep_start);
                while (*cur == ' ') cur++;

                uint32_t status = 0;
                while (*cur >= '0' && *cur <= '9') {
                    status = status * 10 + (*cur - '0');
                    cur++;
                }
                while (*cur == ' ') cur++;

                uint32_t lat = 0;
                while (*cur >= '0' && *cur <= '9') {
                    lat = lat * 10 + (*cur - '0');
                    cur++;
                }

                while (*cur != '\n' && cur < p_end) cur++;
                if (cur < p_end && *cur == '\n') cur++;
                p = cur;

                uint64_t h = hash_bytes(ep_start, ep_len);
                if (h == 0) h = 1;
                size_t slot_idx = h & EP_MASK;
                while (ep_table[slot_idx].hash != h) {
                    slot_idx = (slot_idx + 1) & EP_MASK;
                }
                uint16_t ep_id = ep_table[slot_idx].id;

                uint32_t slot = hour_id * num_endpoints + ep_id;
                counts[slot]++;
                if (status >= 500) errors[slot]++;
                lats.push_back({slot, lat});
            }
        });
    }
    for (auto& w : workers) w.join();
    auto t_parse = std::chrono::high_resolution_clock::now();

    view.close();

    // Merge counts & compute prefix sums
    std::vector<uint32_t> total_counts(num_total_slots, 0);
    std::vector<uint32_t> total_errors(num_total_slots, 0);
    std::vector<size_t> slot_offsets(num_total_slots + 1, 0);

    for (uint32_t s = 0; s < num_total_slots; ++s) {
        uint32_t c = 0, e = 0;
        for (unsigned int tid = 0; tid < nthreads; ++tid) {
            c += thread_counts[tid][s];
            e += thread_errors[tid][s];
        }
        total_counts[s] = c;
        total_errors[s] = e;
        slot_offsets[s + 1] = slot_offsets[s] + c;
    }

    std::vector<std::vector<size_t>> thread_offsets(nthreads, std::vector<size_t>(num_total_slots, 0));
    for (uint32_t s = 0; s < num_total_slots; ++s) {
        size_t base = slot_offsets[s];
        for (unsigned int tid = 0; tid < nthreads; ++tid) {
            thread_offsets[tid][s] = base;
            base += thread_counts[tid][s];
        }
    }
    thread_counts.clear(); thread_counts.shrink_to_fit();

    size_t total_requests = slot_offsets[num_total_slots];
    std::vector<uint32_t> all_lats(total_requests);

    std::vector<std::thread> scatters;
    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        scatters.emplace_back([&, tid]() {
            auto& offsets = thread_offsets[tid];
            for (const auto& item : thread_lats[tid]) {
                all_lats[offsets[item.slot]++] = item.lat;
            }
            thread_lats[tid].clear();
            thread_lats[tid].shrink_to_fit();
        });
    }
    for (auto& t : scatters) t.join();
    auto t_scatter = std::chrono::high_resolution_clock::now();
    thread_offsets.clear(); thread_offsets.shrink_to_fit();

    // Format output in parallel
    std::vector<std::string> chunk_strings(nthreads);
    std::vector<std::thread> formatters;

    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        formatters.emplace_back([&, tid]() {
            uint32_t s_start = tid * num_total_slots / nthreads;
            uint32_t s_end = (tid + 1) * num_total_slots / nthreads;
            std::string& out_buf = chunk_strings[tid];
            out_buf.reserve((s_end - s_start) * 75);

            for (uint32_t s = s_start; s < s_end; ++s) {
                uint32_t n = total_counts[s];
                if (n == 0) continue;

                uint32_t h_id = s / num_endpoints;
                uint32_t ep_id = s % num_endpoints;

                size_t lat_start = slot_offsets[s];
                uint32_t* lats_ptr = all_lats.data() + lat_start;

                std::sort(lats_ptr, lats_ptr + n);

                uint32_t r50 = (50ULL * n + 99) / 100;
                uint32_t r95 = (95ULL * n + 99) / 100;
                uint32_t r99 = (99ULL * n + 99) / 100;

                uint32_t p50 = lats_ptr[r50 - 1];
                uint32_t p95 = lats_ptr[r95 - 1];
                uint32_t p99 = lats_ptr[r99 - 1];

                uint32_t errs = total_errors[s];
                uint64_t error_ppm = (static_cast<uint64_t>(errs) * 1000000ULL) / n;

                out_buf.append(hour_strings[h_id]);
                out_buf.push_back(' ');
                out_buf.append(sorted_eps[ep_id]);
                out_buf.push_back(' ');
                append_uint(out_buf, n);
                out_buf.push_back(' ');
                append_uint(out_buf, p50);
                out_buf.push_back(' ');
                append_uint(out_buf, p95);
                out_buf.push_back(' ');
                append_uint(out_buf, p99);
                out_buf.push_back(' ');
                append_uint(out_buf, errs);
                out_buf.push_back(' ');
                append_uint(out_buf, error_ppm);
                out_buf.push_back('\n');
            }
        });
    }
    for (auto& f : formatters) f.join();

    std::ofstream out(out_path, std::ios::binary);
    for (const auto& s : chunk_strings) {
        out.write(s.data(), s.size());
    }
    out.close();

    auto t_end = std::chrono::high_resolution_clock::now();
    std::cerr << "Map file:     " << std::chrono::duration<double>(t_read - t0).count() * 1000 << " ms\n";
    std::cerr << "Ep discover:  " << std::chrono::duration<double>(t_ep - t_read).count() * 1000 << " ms\n";
    std::cerr << "Main parse:   " << std::chrono::duration<double>(t_parse - t_ep).count() * 1000 << " ms\n";
    std::cerr << "Scatter:      " << std::chrono::duration<double>(t_scatter - t_parse).count() * 1000 << " ms\n";
    std::cerr << "Format & Out: " << std::chrono::duration<double>(t_end - t_scatter).count() * 1000 << " ms\n";
    std::cerr << "Total:        " << std::chrono::duration<double>(t_end - t0).count() * 1000 << " ms\n";

    return 0;
}
