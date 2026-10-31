#include "jit.h"
// discovery.cpp - see discovery.h.
#include "discovery.h"

#include "win32.h"
#include "loader.h"
#if RECOMP_PCODE_FALLBACK
#include "pcode_abi.h"
#endif

#include "../platform/os.h"

#include <cstdlib>
#include <cstring>
#include <map>
#include <mutex>
#include <string>

namespace {

struct Find {
    std::string kind;
    uint32_t from = 0;
    uint32_t hits = 0;
};

std::mutex g_mutex;
std::map<uint32_t, Find> &finds() {
    static auto *m = new std::map<uint32_t, Find>();
    return *m;
}

// The file is opened once, at exit: a run that ends in abort() writes the same
// file as one that returns from its main loop, and a run that finds nothing
// writes an empty file rather than leaving a stale one behind.
const char *path() {
    static const char *value = recomp_env("DISCOVERY");
    return value;
}

bool g_registered = false;

// The next pass hands every recorded address to the translator as an entry
// point, and the translator refuses one that is not in a code section. A call
// through a pointer the guest never filled in arrives here as 0, a wild one as
// whatever the slot held, and a run that has lost its way names whatever it
// reads - data in the image included. Recording any of those would stop the
// next pass dead, so ask the question the translator asks.
bool in_code_section(uint32_t target) {
    const auto *sections = &loader_sections();
    if (const LoaderModule *m = loader_module_containing(target))
        if (m->base != loader_image_base())
            sections = &m->sections;
    for (const SectionInfo &s : *sections) {
        uint32_t size = s.vsize ? s.vsize : s.raw_size;
        if (target >= s.va && target < s.va + size)
            return (s.characteristics & 0x20000000u) != 0; // IMAGE_SCN_MEM_EXECUTE
    }
    return false; // the PE headers, a gap, or no image yet
}

// The caller holds g_mutex.
void write_locked();

} // namespace

extern "C" void discovery_note(const char *kind, uint32_t target, uint32_t from) {
#if RECOMP_PCODE_FALLBACK
    static const bool jit_enabled = recomp_env("JIT_INPUT") && recomp_env("JIT_OUT");
    if (jit_enabled && in_code_section(target) && target >= loader_image_base() &&
        target < loader_image_limit()) {
        static std::once_flag registered;
        std::call_once(registered, [] { atexit(rp_jit_shutdown); });
        rp_jit_note(target, kind && !strcmp(kind, "call"), loader_image_base(), loader_exe_sha256(),
                    recomp_jit_compile);
    }
#endif
    if (!path())
        return;
    if (!in_code_section(target))
        return;
    std::lock_guard<std::mutex> lock(g_mutex);
    if (!g_registered) {
        g_registered = true;
        atexit(discovery_write);
    }
    Find &f = finds()[target];
    bool fresh = f.hits == 0;
    if (fresh) {
        f.kind = kind ? kind : "call";
        f.from = from;
    }
    ++f.hits;
    if (fresh)
        write_locked(); // a run that ends in abort() never reaches atexit
}

extern "C" uint32_t discovery_count(void) {
    std::lock_guard<std::mutex> lock(g_mutex);
    return (uint32_t)finds().size();
}

namespace {
void write_locked() {
    const char *file = path();
    if (!file)
        return;
    FILE *out = fopen(file, "w");
    if (!out) {
        LOGW("discovery: cannot write %s", file);
        return;
    }
    fprintf(out, "# Evidence for the Ghidra analysis and for [translate] discovery_evidence.\n"
                 "# <address> <call|jump> <the instruction that named it> <times reached>\n");
    for (const auto &kv : finds())
        fprintf(out, "%08x %s %08x %u\n", kv.first, kv.second.kind.c_str(), kv.second.from,
                kv.second.hits);
    fclose(out);
}
} // namespace

extern "C" void discovery_write(void) {
    std::lock_guard<std::mutex> lock(g_mutex);
    write_locked();
}
