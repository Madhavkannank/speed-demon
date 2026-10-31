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

struct Edge {
    uint32_t v;
    uint32_t w;
};

static inline uint32_t fast_parse_u32(const char*& p) {
    while (*p <= ' ' && *p != '\0') ++p;
    uint32_t v = 0;
    while (*p >= '0' && *p <= '9') {
        v = v * 10 + (*p - '0');
        ++p;
    }
    return v;
}

struct HeapNode {
    uint64_t d;
    uint32_t u;
};

struct QuadHeap {
    std::vector<HeapNode> data;

    void clear() { data.clear(); }
    bool empty() const { return data.empty(); }
    uint64_t min_key() const { return data.empty() ? UINT64_MAX : data[0].d; }

    void push(uint64_t d, uint32_t u) {
        size_t i = data.size();
        data.push_back({d, u});
        while (i > 0) {
            size_t parent = (i - 1) >> 2;
            if (data[i].d < data[parent].d) {
                std::swap(data[i], data[parent]);
                i = parent;
            } else {
                break;
            }
        }
    }

    HeapNode pop() {
        HeapNode top = data[0];
        HeapNode last = data.back();
        data.pop_back();
        if (!data.empty()) {
            data[0] = last;
            size_t i = 0;
            size_t n = data.size();
            while (true) {
                size_t child = (i << 2) + 1;
                if (child >= n) break;
                size_t min_child = child;
                uint64_t min_d = data[child].d;

                size_t end_child = std::min(child + 4, n);
                for (size_t c = child + 1; c < end_child; ++c) {
                    if (data[c].d < min_d) {
                        min_d = data[c].d;
                        min_child = c;
                    }
                }

                if (min_d < data[i].d) {
                    std::swap(data[i], data[min_child]);
                    i = min_child;
                } else {
                    break;
                }
            }
        }
        return top;
    }
};

int main(int argc, char* argv[]) {
    std::ios_base::sync_with_stdio(false);
    std::cin.tie(nullptr);

    std::string data_dir = argc >= 2 ? argv[1] : "/data";
    std::string out_dir = argc >= 3 ? argv[2] : "/out";

    auto t0 = std::chrono::high_resolution_clock::now();

    FileView gview;
    if (!gview.open(data_dir + "/graph.txt")) return 1;

    const char* gp = gview.data;
    uint32_t n = fast_parse_u32(gp);
    size_t m = fast_parse_u32(gp);

    const char* edges_start = gp;
    const char* g_end = gview.data + gview.size;
    unsigned int nthreads = std::max(1u, std::thread::hardware_concurrency());

    std::vector<const char*> chunk_starts(nthreads + 1);
    chunk_starts[0] = edges_start;
    chunk_starts[nthreads] = g_end;
    size_t edges_len = g_end - edges_start;
    for (unsigned int tid = 1; tid < nthreads; ++tid) {
        const char* target = edges_start + (tid * edges_len / nthreads);
        while (target < g_end && *target != '\n') target++;
        if (target < g_end && *target == '\n') target++;
        chunk_starts[tid] = target;
    }

    struct RawEdge { uint32_t u, v, w; };
    std::vector<std::vector<RawEdge>> thread_edges(nthreads);
    std::vector<std::thread> edge_parsers;
    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        edge_parsers.emplace_back([&, tid]() {
            auto& v = thread_edges[tid];
            v.reserve((m / nthreads) + 8192);
            const char* p = chunk_starts[tid];
            const char* p_end = chunk_starts[tid + 1];
            while (p < p_end) {
                while (p < p_end && *p <= ' ') p++;
                if (p >= p_end) break;
                uint32_t u = 0, v_node = 0, w = 0;
                while (*p >= '0' && *p <= '9') { u = u * 10 + (*p - '0'); p++; }
                while (*p <= ' ') p++;
                while (*p >= '0' && *p <= '9') { v_node = v_node * 10 + (*p - '0'); p++; }
                while (*p <= ' ') p++;
                while (*p >= '0' && *p <= '9') { w = w * 10 + (*p - '0'); p++; }
                v.push_back({u, v_node, w});
            }
        });
    }
    for (auto& t : edge_parsers) t.join();
    gview.close();

    std::vector<uint32_t> head_fwd(n + 2, 0);
    std::vector<uint32_t> head_rev(n + 2, 0);

    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        for (const auto& e : thread_edges[tid]) {
            head_fwd[e.u + 1]++;
            head_rev[e.v + 1]++;
        }
    }

    for (uint32_t i = 0; i <= n; ++i) {
        head_fwd[i + 1] += head_fwd[i];
        head_rev[i + 1] += head_rev[i];
    }

    std::vector<uint32_t> cur_pos_fwd = head_fwd;
    std::vector<uint32_t> cur_pos_rev = head_rev;
    std::vector<Edge> csr_fwd(m);
    std::vector<Edge> csr_rev(m);

    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        for (const auto& e : thread_edges[tid]) {
            csr_fwd[cur_pos_fwd[e.u]++] = {e.v, e.w};
            csr_rev[cur_pos_rev[e.v]++] = {e.u, e.w};
        }
        thread_edges[tid].clear();
        thread_edges[tid].shrink_to_fit();
    }
    cur_pos_fwd.clear(); cur_pos_fwd.shrink_to_fit();
    cur_pos_rev.clear(); cur_pos_rev.shrink_to_fit();

    auto t_graph = std::chrono::high_resolution_clock::now();

    FileView qview;
    if (!qview.open(data_dir + "/queries.txt")) return 1;

    const char* qp = qview.data;
    std::vector<std::pair<uint32_t, uint32_t>> queries;
    while (*qp) {
        while (*qp <= ' ' && *qp != '\0') ++qp;
        if (!*qp) break;
        uint32_t s = fast_parse_u32(qp);
        uint32_t t = fast_parse_u32(qp);
        queries.push_back({s, t});
    }

    size_t num_q = queries.size();
    std::vector<std::string> thread_outputs(nthreads);
    std::vector<std::thread> workers;

    for (unsigned int tid = 0; tid < nthreads; ++tid) {
        workers.emplace_back([&, tid]() {
            size_t start = tid * num_q / nthreads;
            size_t end = (tid + 1) * num_q / nthreads;

            std::vector<uint64_t> dist_f(n, UINT64_MAX);
            std::vector<uint64_t> dist_r(n, UINT64_MAX);
            std::vector<uint32_t> visited_f;
            std::vector<uint32_t> visited_r;
            visited_f.reserve(8192);
            visited_r.reserve(8192);

            QuadHeap pq_f;
            QuadHeap pq_r;
            pq_f.data.reserve(4096);
            pq_r.data.reserve(4096);

            std::string& out = thread_outputs[tid];
            out.reserve((end - start) * 16);
            char buf[32];

            for (size_t qi = start; qi < end; ++qi) {
                uint32_t s = queries[qi].first, t = queries[qi].second;
                if (s == t) {
                    out.append("0\n");
                    continue;
                }

                if (head_fwd[s + 1] == head_fwd[s] || head_rev[t + 1] == head_rev[t]) {
                    out.append("-1\n");
                    continue;
                }

                dist_f[s] = 0; visited_f.push_back(s);
                dist_r[t] = 0; visited_r.push_back(t);
                pq_f.push(0, s);
                pq_r.push(0, t);

                uint64_t best_dist = UINT64_MAX;

                while (!pq_f.empty() && !pq_r.empty()) {
                    uint64_t min_f = pq_f.min_key();
                    uint64_t min_r = pq_r.min_key();
                    if (min_f + min_r >= best_dist) {
                        break;
                    }

                    if (min_f <= min_r) {
                        HeapNode top = pq_f.pop();
                        if (top.d > dist_f[top.u]) continue;
                        if (top.d >= best_dist) break;

                        if (dist_r[top.u] != UINT64_MAX) {
                            uint64_t total = top.d + dist_r[top.u];
                            if (total < best_dist) best_dist = total;
                        }

                        uint32_t e_end = head_fwd[top.u + 1];
                        for (uint32_t e = head_fwd[top.u]; e < e_end; ++e) {
                            uint32_t v = csr_fwd[e].v;
                            uint64_t nd = top.d + csr_fwd[e].w;
                            if (nd < best_dist && nd < dist_f[v]) {
                                if (dist_f[v] == UINT64_MAX) visited_f.push_back(v);
                                dist_f[v] = nd;
                                pq_f.push(nd, v);
                                if (dist_r[v] != UINT64_MAX) {
                                    uint64_t total = nd + dist_r[v];
                                    if (total < best_dist) best_dist = total;
                                }
                            }
                        }
                    } else {
                        HeapNode top = pq_r.pop();
                        if (top.d > dist_r[top.u]) continue;
                        if (top.d >= best_dist) break;

                        if (dist_f[top.u] != UINT64_MAX) {
                            uint64_t total = top.d + dist_f[top.u];
                            if (total < best_dist) best_dist = total;
                        }

                        uint32_t e_end = head_rev[top.u + 1];
                        for (uint32_t e = head_rev[top.u]; e < e_end; ++e) {
                            uint32_t v = csr_rev[e].v;
                            uint64_t nd = top.d + csr_rev[e].w;
                            if (nd < best_dist && nd < dist_r[v]) {
                                if (dist_r[v] == UINT64_MAX) visited_r.push_back(v);
                                dist_r[v] = nd;
                                pq_r.push(nd, v);
                                if (dist_f[v] != UINT64_MAX) {
                                    uint64_t total = nd + dist_f[v];
                                    if (total < best_dist) best_dist = total;
                                }
                            }
                        }
                    }
                }

                pq_f.clear();
                pq_r.clear();
                for (uint32_t u : visited_f) dist_f[u] = UINT64_MAX;
                for (uint32_t u : visited_r) dist_r[u] = UINT64_MAX;
                visited_f.clear();
                visited_r.clear();

                int64_t ans = (best_dist == UINT64_MAX) ? -1 : static_cast<int64_t>(best_dist);
                auto [ptr, ec] = std::to_chars(buf, buf + sizeof(buf), ans);
                out.append(buf, ptr - buf);
                out.push_back('\n');
            }
        });
    }

    for (auto& w : workers) w.join();
    auto t_dijk = std::chrono::high_resolution_clock::now();

    std::ofstream out(out_dir + "/distances.txt", std::ios::binary);
    for (const auto& s : thread_outputs) {
        out.write(s.data(), s.size());
    }
    out.close();

    return 0;
}
