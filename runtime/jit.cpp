#include "jit.h"
#include "loader.h"
#include "guest.h"
#include "seh.h"
#include "jit_abi.h"
#include "../platform/os.h"

#include <cstdio>
#include <cstring>
#include <cstdlib>
#include <memory>
#include <unordered_map>
#include <string>
#include <vector>

namespace {
struct Loaded {
    void *handle;
    const RecompJitModule *module;
    bool valid;
    bool entered;
    std::string directory;
    std::vector<uint64_t> calls;
    std::vector<uint64_t> handoffs;
};
struct Entry {
    Loaded *loaded;
    RecompEntryFn fn;
    uint32_t index;
};
struct Registry {
    std::vector<std::unique_ptr<Loaded>> modules;
    std::unordered_map<uint32_t, Entry> entries;
};
struct Jump {
    X86 *cpu = nullptr;
    uint32_t target = 0;
    bool pending = false;
};
thread_local Jump jump;
extern "C" int recomp_thunk_target_kind(uint32_t);
void report();
Registry &registry() {
    static auto *value = [] {
        atexit(report);
        return new Registry;
    }();
    return *value;
}

void module_status(const Loaded &loaded) {
#if RECOMP_PCODE_FALLBACK
    std::vector<uint32_t> addresses;
    for (uint32_t i = 0; i < loaded.module->entry_count; ++i)
        addresses.push_back(loaded.module->entries[i].address);
    rp_jit_module_status(loaded.directory.c_str(), loaded.valid ? "published" : "invalidated",
                         loaded.valid ? nullptr : "guest code bytes changed", addresses.data(),
                         loaded.calls.data(), loaded.handoffs.data(),
                         (loaded.module->entries[0].metadata & RECOMP_JIT_RESUME_SAFE) != 0,
                         (uint32_t)addresses.size());
#endif
}

void report() {
    for (const auto &loaded : registry().modules)
        module_status(*loaded);
}

bool bytes_match(const RecompJitModule *m) {
    for (uint32_t i = 0; i < m->range_count; ++i) {
        const auto &r = m->ranges[i];
        if (!r.bytes || !r.size || !gm_valid(r.address, r.size) ||
            memcmp(g_mem + r.address, r.bytes, r.size))
            return false;
    }
    return true;
}

const char *descriptor_error(const RecompJitModule *m) {
    if (!m)
        return "missing descriptor";
    if (m->version != 1 || m->cpu_size != sizeof(X86))
        return "ABI mismatch";
    if (m->image_base != loader_image_base() || !m->image_sha256 ||
        strcmp(m->image_sha256, loader_exe_sha256()))
        return "image mismatch";
    const uint32_t image_size = loader_image_limit() - loader_image_base();
    if (!m->entry_count || m->entry_count > image_size || !m->range_count ||
        m->range_count > image_size || m->return_count > image_size || !m->entries || !m->ranges ||
        (m->return_count && !m->returns))
        return "invalid descriptor counts or arrays";
    for (uint32_t i = 0; i < m->entry_count; ++i) {
        const auto &e = m->entries[i];
        if (!e.fn || (e.metadata & ~RECOMP_JIT_RESUME_SAFE) ||
            (i && e.address <= m->entries[i - 1].address))
            return "invalid or unsorted entry";
        bool covered = false;
        for (uint32_t j = 0; j < m->range_count; ++j)
            covered |= e.address >= m->ranges[j].address &&
                       e.address - m->ranges[j].address < m->ranges[j].size;
        if (!covered)
            return "entry outside validated code";
    }
    return bytes_match(m) ? nullptr : "invalid source ranges or stale guest bytes";
}

void publish() {
#if RECOMP_PCODE_FALLBACK
    while (char *directory = rp_jit_take()) {
        std::string dir = directory;
        std::string path = dir + "/module" + os_plugin_extension();
        rp_jit_free_path(directory);
        void *handle = os_dlopen(path.c_str());
        if (!handle) {
            const char *error = os_dlerror();
            fprintf(stderr, "[recomp] jit: load %s: %s\n", path.c_str(), error);
            rp_jit_module_status(dir.c_str(), "load_failed", error, nullptr, nullptr, nullptr, 0,
                                 0);
            continue;
        }
        auto get = reinterpret_cast<const RecompJitModule *(*)()>(
            os_dlsym(handle, "recomp_jit_descriptor"));
        const RecompJitModule *m = get ? get() : nullptr;
        if (const char *error = descriptor_error(m)) {
            fprintf(stderr, "[recomp] jit: rejected %s: %s\n", path.c_str(), error);
            rp_jit_module_status(dir.c_str(), "rejected", error, nullptr, nullptr, nullptr, 0, 0);
            os_dlclose(handle);
            continue;
        }
        auto &r = registry();
        auto loaded = std::make_unique<Loaded>(Loaded{handle, m, true, false, dir,
                                                      std::vector<uint64_t>(m->entry_count),
                                                      std::vector<uint64_t>(m->entry_count)});
        for (uint32_t i = 0; i < m->entry_count; ++i)
            r.entries.try_emplace(m->entries[i].address, Entry{loaded.get(), m->entries[i].fn, i});
        module_status(*loaded);
        r.modules.push_back(std::move(loaded));
        fprintf(stderr, "[recomp] jit: published %u entries from %s\n", m->entry_count,
                path.c_str());
    }
#endif
}
} // namespace

int recomp_jit_compile(const char *input, const char *directory) {
#if RECOMP_PCODE_FALLBACK
    const char *argv[] = {RECOMP_JIT_PYTHON,
                          RECOMP_JIT_BUILD_SCRIPT,
                          "--jit-module",
                          directory,
                          "--jit-input",
                          input,
                          nullptr};
    int64_t pid;
    int status;
    if (os_spawn(argv, &pid) || os_wait(pid, &status))
        return -1;
    return status;
#else
    return -1;
#endif
}

namespace {
int invoke(X86 *c, uint32_t target, uint32_t continuation, uint32_t *next, bool resume) {
    static const bool disabled = [] {
        const char *mode = getenv("RECOMP_JIT_EXEC");
        return mode && !strcmp(mode, "off");
    }();
    if (disabled)
        return 0;
    publish();
    const unsigned depth = c->dynamic_return_count;
    bool entered = false;
    for (;;) {
        auto &r = registry();
        auto found = r.entries.find(target);
        if (found == r.entries.end() || !found->second.loaded->valid ||
            (!entered && resume &&
             !(found->second.loaded->module->entries[found->second.index].metadata &
               RECOMP_JIT_RESUME_SAFE))) {
            if (!entered)
                return 0;
            recomp_dynamic_return_pop(c, depth);
            *next = target;
            return 2;
        }
        Loaded *loaded = found->second.loaded;
        RecompEntryFn fn = found->second.fn;
        uint32_t index = found->second.index;
        if (!bytes_match(loaded->module)) {
            loaded->valid = false;
            module_status(*loaded);
            fprintf(stderr, "[recomp] jit: invalidated batch at %08x\n", target);
            continue;
        }
        if (!entered) {
            if (resume && continuation)
                recomp_dynamic_return_push(c, continuation);
            if (resume) {
                ++loaded->handoffs[index];
                if (loaded->handoffs[index] == 1)
                    fprintf(stderr,
                            "[recomp] jit: interpreter branch handoff %08x (continuation %08x)\n",
                            target, continuation);
            }
        }
        entered = true;
        ++loaded->calls[index];
        if (!loaded->entered) {
            loaded->entered = true;
            module_status(*loaded);
            fprintf(stderr, "[recomp] jit: native entry %08x\n", target);
        }
        recomp_execution_checkpoint(c);
        const Jump previous = jump;
        jump = Jump{c, 0, false};
        fn(c);
        const Jump result = jump;
        jump = previous;
        if (!result.pending || result.cpu != c) {
            recomp_dynamic_return_pop(c, depth);
            return 1;
        }
        target = result.target;
        c->eip = target;
        if (continuation && target == continuation) {
            recomp_dynamic_return_pop(c, depth);
            return 1;
        }
        if (recomp_thunk_target_kind(target) == 1 ||
            (target >= GUEST_SHIM_BASE && target < GUEST_SHIM_END)) {
            recomp_dynamic_return_pop(c, depth);
            *next = target;
            return 2;
        }
    }
}
} // namespace

void recomp_jit_jump(X86 *c, uint32_t target) {
    if (recomp_seh_pending_target()) {
        recomp_jump(c, target);
        return;
    }
    jump = Jump{c, target, true};
    c->eip = target;
}

int recomp_jit_call(X86 *c, uint32_t target) {
    uint32_t next = 0;
    const uint32_t continuation = gm_valid(c->r[R_ESP], 4) ? rd32(c->r[R_ESP]) : 0;
    const int result = invoke(c, target, continuation, &next, false);
    if (result == 2)
        recomp_jump(c, next);
    return result != 0;
}

int recomp_jit_resume(X86 *c, uint32_t target, uint32_t continuation, uint32_t *next) {
    return invoke(c, target, continuation, next, true);
}
