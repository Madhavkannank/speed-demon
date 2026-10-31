#include <iostream>
#include <fstream>
#include <vector>
#include <string>
#include <thread>
#include <algorithm>
#include <cstdint>
#include <cstring>
#ifdef _WIN32
#include <windows.h>
struct PFile { HANDLE h=INVALID_HANDLE_VALUE; int64_t sz=0; bool open(const std::string& p){ h=CreateFileA(p.c_str(),GENERIC_READ,FILE_SHARE_READ,NULL,OPEN_EXISTING,FILE_FLAG_SEQUENTIAL_SCAN,NULL); if(h==INVALID_HANDLE_VALUE)return false; LARGE_INTEGER li; GetFileSizeEx(h,&li); sz=li.QuadPart; return true; } int64_t read_at(void* b, size_t c, int64_t o){ OVERLAPPED ov={0}; ov.Offset=(DWORD)(o&0xFFFFFFFF); ov.OffsetHigh=(DWORD)((o>>32)&0xFFFFFFFF); DWORD r=0; return ReadFile(h,b,(DWORD)c,&r,&ov)?r:-1; } void close(){ if(h!=INVALID_HANDLE_VALUE){CloseHandle(h); h=INVALID_HANDLE_VALUE;} } ~PFile(){ close(); } };
static inline int ctz64(uint64_t x){ unsigned long r; _BitScanForward64(&r,x); return (int)r; }
#else
#include <unistd.h>
#include <fcntl.h>
#include <sys/stat.h>
struct PFile { int fd=-1; int64_t sz=0; bool open(const std::string& p){ fd=::open(p.c_str(),O_RDONLY); if(fd<0)return false; struct stat st; fstat(fd,&st); sz=st.st_size; return true; } int64_t read_at(void* b, size_t c, int64_t o){ return pread(fd,b,c,o); } void close(){ if(fd>=0){::close(fd); fd=-1;} } ~PFile(){ close(); } };
static inline int ctz64(uint64_t x){ return __builtin_ctzll(x); }
#endif
static inline uint32_t fast_hash(const char* p, size_t len){ uint32_t h=0x517cc1b7u; size_t i=0; while(i+8<=len){ uint64_t w; std::memcpy(&w,p+i,8); h=(((h<<7)|(h>>25))^(uint32_t)w^(uint32_t)(w>>32))*0x27220a95u; i+=8; } if(i<len){ uint64_t r=0; std::memcpy(&r,p+i,len-i); h=(((h<<7)|(h>>25))^(uint32_t)r^(uint32_t)(r>>32))*0x27220a95u; } return (h^((uint32_t)len*0x9e3779b9u))*0x27220a95u; }
static inline uint32_t get_hour_idx(const char* ts){ uint32_t d=(ts[8]-'0')*10+(ts[9]-'0'), h=(ts[11]-'0')*10+(ts[12]-'0'); return d==30?(h>=18?h-18:0):(d==31?6+h:(d==1?30+h:54+h)); }
static inline void get_hour_str(uint32_t h, char* o){ uint32_t d=h<=5?30:(h<=29?31:(h<=53?1:2)), hr=h<=5?18+h:(h<=29?h-6:(h<=53?h-30:h-54)); std::memcpy(o,"2026-10-00T00:00:00Z",20); if(d<30)o[6]='1'; o[8]='0'+(d/10); o[9]='0'+(d%10); o[11]='0'+(hr/10); o[12]='0'+(hr%10); }
static inline char* put_u32(char* p, uint32_t n){ if(!n){*p++='0'; return p;} char b[10]; int i=10; while(n){b[--i]='0'+(n%10); n/=10;} while(i<10)*p++=b[i++]; return p; }
struct HSlot { uint32_t hash; uint16_t len, id; }; struct GEntry { uint32_t stat, head; }; struct LChunk { uint32_t lats[14]; uint16_t count; uint32_t next; };
struct WConfig { uint32_t base_h, num_h; }; const size_t NW=12, CSZ=128*1024, TSZ=4096, TMSK=TSZ-1; const WConfig CFG[NW]={{0,5},{4,12},{15,6},{20,5},{23,6},{28,12},{39,6},{44,5},{47,6},{52,12},{63,6},{68,4}}; struct WOutput { uint32_t base_h, num_h; std::vector<GEntry> groups; std::vector<LChunk> chunks; };
int main(int argc, char* argv[]){
    std::string dd=argc>=2?argv[1]:(std::getenv("DATA_DIR")?std::getenv("DATA_DIR"):"/data"), od=argc>=3?argv[2]:(std::getenv("OUT_DIR")?std::getenv("OUT_DIR"):"/out"), pth=dd+"/requests.log", opth=od+"/summary.txt";
    PFile f; if(!f.open(pth))return 1; if(!f.sz){ std::ofstream(opth).close(); return 0; }
    std::vector<HSlot> pt(TSZ,{0,0,0}); std::vector<std::string> eps; eps.reserve(2048); size_t iscan=(size_t)std::min<int64_t>(8*1024*1024, f.sz); std::vector<char> sbuf(iscan+1024); int64_t nr=f.read_at(sbuf.data(),iscan,0);
    if(nr>0){ size_t lnl=(size_t)nr; while(lnl>0 && sbuf[lnl-1]!='\n')lnl--; size_t pos=0; while(pos+35<=lnl && eps.size()<1842){ size_t idx=pos+28; while(idx<lnl && sbuf[idx]!=' ')idx++; size_t ep_s=idx+1; idx=ep_s; while(idx<lnl && sbuf[idx]!=' ')idx++; size_t elen=idx-ep_s; uint32_t h=fast_hash(sbuf.data()+ep_s, elen), slot=h&TMSK; while(true){ auto& e=pt[slot]; if(e.hash==h && e.len==elen)break; if(!e.len){ e={h,(uint16_t)elen,(uint16_t)eps.size()}; eps.emplace_back(sbuf.data()+ep_s,elen); break; } slot=(slot+1)&TMSK; } while(idx<lnl && sbuf[idx]!='\n')idx++; pos=idx+1; } }
    sbuf.clear(); sbuf.shrink_to_fit(); pt.clear(); pt.shrink_to_fit(); std::sort(eps.begin(), eps.end()); uint32_t ne=(uint32_t)eps.size();
    std::vector<HSlot> et(TSZ,{0,0,0}); for(uint32_t i=0;i<ne;++i){ uint32_t h=fast_hash(eps[i].data(),eps[i].size()), slot=h&TMSK; while(et[slot].len)slot=(slot+1)&TMSK; et[slot]={h,(uint16_t)eps[i].size(),(uint16_t)i}; }
    int64_t achunk=f.sz/NW; std::vector<int64_t> cb(NW+1,0); std::vector<char> scb(1024); for(size_t i=1;i<NW;++i){ f.read_at(scb.data(),scb.size(),i*achunk); int64_t o=0; while(o<1024 && scb[o]!='\n')o++; cb[i]=i*achunk+o+1; } cb[NW]=f.sz;
    std::vector<WOutput> wout(NW); std::vector<std::thread> tp; for(size_t t=0;t<NW;++t){ tp.emplace_back([&,t](){
        auto& w=wout[t]; w.base_h=CFG[t].base_h; w.num_h=CFG[t].num_h; w.groups.assign(w.num_h*ne,{0,0}); w.chunks.reserve(18000); int64_t coff=cb[t], eoff=cb[t+1]; std::vector<char> buf(CSZ+4096); size_t rlen=0; char lh[13]={0}; uint32_t chb=0;
        while(coff<eoff || rlen>0){ size_t tr=(size_t)std::min<int64_t>(CSZ, eoff-coff), rb=0; if(tr>0){ int64_t n=f.read_at(buf.data()+rlen,tr,coff); if(n>0){coff+=n; rb=(size_t)n;} } size_t av=rlen+rb; if(!av)break; size_t lnl=av; while(lnl>0 && buf[lnl-1]!='\n')lnl--; if(!lnl)break; const char* s=buf.data(); size_t pos=0;
        while(pos+35<=lnl){ uint32_t hb; if(!std::memcmp(s+pos,lh,13)) hb=chb; else{ std::memcpy(lh,s+pos,13); uint32_t hi=get_hour_idx(s+pos); chb=(hi>=w.base_h)?(hi-w.base_h)*ne:0; hb=chb; }
        char m0=s[pos+28]; size_t es=pos+28+(m0=='G'?4:(m0=='P'?(s[pos+29]=='U'?4:(s[pos+29]=='O'?5:6)):(m0=='H'?5:(m0=='D'?7:0)))), idx=es; if(m0!='G'&&m0!='P'&&m0!='H'&&m0!='D'){while(s[es]!=' ')es++; es++; idx=es;}
        uint32_t h=0x517cc1b7u; while(true){ uint64_t w64; std::memcpy(&w64,s+idx,8); uint64_t m=(w64^0x2020202020202020ULL)-0x0101010101010101ULL&~(w64^0x2020202020202020ULL)&0x8080808080808080ULL; if(m){ int sp=ctz64(m)>>3; if(sp>0){ uint64_t rw=w64&((1ULL<<(sp*8))-1ULL); h=(((h<<7)|(h>>25))^(uint32_t)rw^(uint32_t)(rw>>32))*0x27220a95u; } idx+=sp; break; } h=(((h<<7)|(h>>25))^(uint32_t)w64^(uint32_t)(w64>>32))*0x27220a95u; idx+=8; }
        size_t elen=idx-es; h=(h^((uint32_t)elen*0x9e3779b9u))*0x27220a95u; size_t slot=h&TMSK; uint16_t eid; while(true){ const auto& e=et[slot]; if(e.hash==h && e.len==elen){eid=e.id; break;} slot=(slot+1)&TMSK; }
        uint32_t err=(s[idx+1]=='5'); const char* p=s+idx+5; uint32_t lat=(*p++-'0'); while(*p!='\n') lat=lat*10+(*p++-'0'); pos=(p-s)+1; auto& g=w.groups[hb+eid]; uint32_t cnt=g.stat&0xFFFF; g.stat+=1+(err<<16);
        if(!cnt) g.head=lat; else if(cnt==1){ uint32_t pl=g.head, ni=(uint32_t)w.chunks.size(); w.chunks.push_back({{pl,lat},2,UINT32_MAX}); g.head=ni; } else{ uint32_t ci=g.head; if(w.chunks[ci].count<14) w.chunks[ci].lats[w.chunks[ci].count++]=lat; else{ uint32_t ni=(uint32_t)w.chunks.size(); w.chunks.push_back({{lat},1,g.head}); g.head=ni; } } }
        rlen=av-lnl; if(rlen>0) std::memmove(buf.data(),buf.data()+lnl,rlen); } }); }
    for(auto& t:tp) t.join(); f.close();
    std::vector<char> ob(NW*900000); std::vector<size_t> osz(NW,0); std::vector<std::thread> otp;
    for(size_t t=0;t<NW;++t){ otp.emplace_back([&,t](){
        uint32_t hs=(uint32_t)(t*6), he=hs+6; char* op=ob.data()+t*900000; std::vector<uint32_t> sc; sc.reserve(4096); char hstr[21];
        for(uint32_t hid=hs; hid<he; ++hid){ get_hour_str(hid,hstr); std::vector<const WOutput*> aw; for(const auto& w:wout) if(hid>=w.base_h && hid<w.base_h+w.num_h) aw.push_back(&w);
        for(uint32_t eid=0; eid<ne; ++eid){ uint32_t tc=0, terr=0; for(const auto* w:aw){ const auto& g=w->groups[(hid-w->base_h)*ne+eid]; tc+=g.stat&0xFFFF; terr+=g.stat>>16; } if(!tc)continue;
        uint32_t p50, p95, p99; if(tc==1){ for(const auto* w:aw){ const auto& g=w->groups[(hid-w->base_h)*ne+eid]; if(g.stat&0xFFFF){p50=p95=p99=g.head; break;} } }
        else{ sc.clear(); for(const auto* w:aw){ const auto& g=w->groups[(hid-w->base_h)*ne+eid]; uint32_t c=g.stat&0xFFFF; if(c==1) sc.push_back(g.head); else if(c>1){ uint32_t cur=g.head; while(cur!=UINT32_MAX){ const auto& ch=w->chunks[cur]; for(uint16_t i=0;i<ch.count;++i) sc.push_back(ch.lats[i]); cur=ch.next; } } }
        size_t n=sc.size(); if(n==2){ uint32_t a=sc[0], b=sc[1]; if(a>b)std::swap(a,b); p50=a; p95=b; p99=b; } else{ std::sort(sc.begin(),sc.end()); p50=sc[(50*n+99)/100-1]; p95=sc[(95*n+99)/100-1]; p99=sc[(99*n+99)/100-1]; } }
        uint32_t ppm=(uint32_t)((uint64_t)terr*1000000ULL/tc); std::memcpy(op,hstr,20); op+=20; *op++=' '; std::memcpy(op,eps[eid].data(),eps[eid].size()); op+=eps[eid].size(); *op++=' ';
        op=put_u32(op,tc); *op++=' '; op=put_u32(op,p50); *op++=' '; op=put_u32(op,p95); *op++=' '; op=put_u32(op,p99); *op++=' '; op=put_u32(op,terr); *op++=' '; op=put_u32(op,ppm); *op++='\n'; } }
        osz[t]=op-(ob.data()+t*900000); }); }
    for(auto& t:otp) t.join(); std::ofstream of(opth,std::ios::binary); for(size_t t=0;t<NW;++t) of.write(ob.data()+t*900000,osz[t]); of.close(); return 0; }
