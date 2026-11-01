#include "seh.h"
#include "loader.h"
#include "pe.h"

#include "memory.h"
#include "imports.h"
#include "win32.h"

#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <cctype>
#include <vector>
#include <string>
#include <algorithm>
#include "../platform/os.h"

const char *const LOADER_DEFAULT_EXE = RECOMP_DEVELOPER_EXE;
// The digest of the image actually mapped, recorded by loader_load.
static std::string g_exe_sha;

const char *const LOADER_EXPECTED_SHA256 = RECOMP_EXE_SHA256;

namespace {

std::string g_error;
std::string g_exe_path;
bool g_interpreted_dispatch = false;
uint32_t g_base = 0, g_size = 0, g_entry = 0, g_iat_patched = 0, g_iat_data = 0;
std::vector<SectionInfo> g_sections;
LoaderModule g_main;
X86 g_ctx;
LoaderTls g_tls = {0, 0, 0, 0, 0, 0xffffffffu};

// --------------------------------------------------------------------------
// SHA-256 (FIPS 180-4), just enough to fingerprint the image.
// --------------------------------------------------------------------------
struct Sha256 {
    uint32_t h[8] = {0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
                     0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19};
    uint64_t len = 0;
    uint8_t buf[64];
    size_t have = 0;

    static uint32_t ror(uint32_t x, int n) {
        return (x >> n) | (x << (32 - n));
    }

    void block(const uint8_t *p) {
        static const uint32_t k[64] = {
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2};
        uint32_t w[64];
        for (int i = 0; i < 16; ++i)
            w[i] = ((uint32_t)p[i * 4] << 24) | ((uint32_t)p[i * 4 + 1] << 16) |
                   ((uint32_t)p[i * 4 + 2] << 8) | (uint32_t)p[i * 4 + 3];
        for (int i = 16; i < 64; ++i) {
            uint32_t s0 = ror(w[i - 15], 7) ^ ror(w[i - 15], 18) ^ (w[i - 15] >> 3);
            uint32_t s1 = ror(w[i - 2], 17) ^ ror(w[i - 2], 19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16] + s0 + w[i - 7] + s1;
        }
        uint32_t a = h[0], b = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g = h[6], hh = h[7];
        for (int i = 0; i < 64; ++i) {
            uint32_t S1 = ror(e, 6) ^ ror(e, 11) ^ ror(e, 25);
            uint32_t ch = (e & f) ^ (~e & g);
            uint32_t t1 = hh + S1 + ch + k[i] + w[i];
            uint32_t S0 = ror(a, 2) ^ ror(a, 13) ^ ror(a, 22);
            uint32_t maj = (a & b) ^ (a & c) ^ (b & c);
            uint32_t t2 = S0 + maj;
            hh = g;
            g = f;
            f = e;
            e = d + t1;
            d = c;
            c = b;
            b = a;
            a = t1 + t2;
        }
        h[0] += a;
        h[1] += b;
        h[2] += c;
        h[3] += d;
        h[4] += e;
        h[5] += f;
        h[6] += g;
        h[7] += hh;
    }

    void update(const uint8_t *p, size_t n) {
        len += n;
        while (n) {
            size_t take = 64 - have;
            if (take > n)
                take = n;
            memcpy(buf + have, p, take);
            have += take;
            p += take;
            n -= take;
            if (have == 64) {
                block(buf);
                have = 0;
            }
        }
    }

    std::string hex() {
        uint64_t bits = len * 8;
        uint8_t pad = 0x80;
        update(&pad, 1);
        uint8_t z = 0;
        while (have != 56)
            update(&z, 1);
        uint8_t tail[8];
        for (int i = 0; i < 8; ++i)
            tail[i] = (uint8_t)(bits >> (56 - 8 * i));
        update(tail, 8);
        char out[65];
        for (int i = 0; i < 8; ++i)
            snprintf(out + i * 8, 9, "%08x", h[i]);
        return std::string(out, 64);
    }
};

std::string sha256_hex(const std::vector<uint8_t> &data) {
    Sha256 s;
    s.update(data.data(), data.size());
    return s.hex();
}

bool read_file(const char *path, std::vector<uint8_t> &out) {
    FILE *f = fopen(path, "rb");
    if (!f)
        return false;
    fseek(f, 0, SEEK_END);
    long n = ftell(f);
    fseek(f, 0, SEEK_SET);
    if (n <= 0) {
        fclose(f);
        return false;
    }
    out.resize((size_t)n);
    size_t got = fread(out.data(), 1, (size_t)n, f);
    fclose(f);
    return got == (size_t)n;
}

std::string dirname_of(const std::string &p) {
    size_t s = p.find_last_of("\\/");
    if (s == std::string::npos)
        return ".";
    if (s == 0)
        return "/";
    return p.substr(0, s);
}

template <typename T> T rd(const std::vector<uint8_t> &d, size_t off) {
    T v{};
    if (off + sizeof(T) <= d.size())
        memcpy(&v, d.data() + off, sizeof(T));
    return v;
}

constexpr size_t PE32_DIRECTORY_COUNT = 92;
constexpr size_t PE32_DIRECTORIES = 96;
constexpr uint32_t DIRECTORY_IMPORT = 1;
constexpr uint32_t DIRECTORY_BOUND_IMPORT = 11;
constexpr uint32_t DIRECTORY_DELAY_IMPORT = 13;
constexpr uint32_t DIRECTORY_CLR = 14;
constexpr uint32_t FIXED_RUNTIME_BASE = 0x0e000000u;

uint32_t directory_count(const std::vector<uint8_t> &file, size_t opt_off) {
    return rd<uint32_t>(file, opt_off + PE32_DIRECTORY_COUNT);
}

// Game images trust their shim tables, logging-only stubs included; an
// arbitrary image binds only implemented imports and rejects malformed tables.
enum class ImagePolicy { Game, Arbitrary };

// True when [rva, rva+len) lies inside an image of `size` bytes.
bool rva_in(uint32_t size, uint32_t rva, uint32_t len) {
    return rva < size && len <= size - rva;
}
// True when [rva, rva+len) lies inside the mapped main image.
bool rva_ok(uint32_t rva, uint32_t len) {
    return rva_in(g_size, rva, len);
}

// Resolve the PE import table of the image mapped at [base, base+size) to
// runtime trampolines or registered guest data storage. Bound every descriptor
// and RVA by the image before reading or patching it.
// --------------------------------------------------------------------------
// Auxiliary modules: DLLs game.toml names, mapped beside the main image at
// their configured base, above the shim arena block, verified by content hash
// like the image and rebased from the PE's preferred base with the image's own
// relocation table. // Preserve configured installation subfolders when the executable and its game
// directory have moved onto another device.
// --------------------------------------------------------------------------
struct AuxSpec {
    const char *name, *path, *sha256;
    uint32_t base, size;
    const char *aliases;
};
const AuxSpec g_aux_specs[] = RECOMP_AUX_MODULES;
std::vector<LoaderModule> g_aux;

// A mapped module whose IAT is not bound yet. Binding waits until every module
// is mapped, so an import from one can see another's exports whatever order
// game.toml lists them in.
struct AuxPending {
    const AuxSpec *spec;
    std::vector<uint8_t> file;
    size_t opt_off;
    uint16_t opt_magic;
    uint32_t size_image;
    uint32_t index; // position in g_aux, which is this vector's position
    std::vector<PeImportModule> imports;
};
std::vector<AuxPending> g_aux_pending;
std::vector<uint32_t> g_aux_order;        // g_aux indices in DllMain dependency order
std::vector<std::vector<int>> g_aux_deps; // g_aux[i]'s auxiliary-module dependencies
// g_aux[i] is attached before the exe entry (it is in the executable's static
// import closure). Runtime-only modules stay false and attach at LoadLibrary.
std::vector<bool> g_aux_startup;

// The basename, lowercased, with a .dll suffix when it has no extension. Two
// spellings of the same import module compare equal through this.
std::string normalize_module_name(const char *raw) {
    std::string n = raw ? raw : "";
    size_t s = n.find_last_of("\\/");
    if (s != std::string::npos)
        n = n.substr(s + 1);
    for (char &ch : n)
        ch = (char)tolower((unsigned char)ch);
    if (n.find('.') == std::string::npos)
        n += ".dll";
    return n;
}

bool answers_to(const LoaderModule &m, const std::string &normalized) {
    if (normalize_module_name(m.name.c_str()) == normalized)
        return true;
    for (const std::string &alias : m.aliases)
        if (normalize_module_name(alias.c_str()) == normalized)
            return true;
    return false;
}

// Add `base - preferred` to every HIGHLOW relocation site in the mapped module.
// The translator's Image applies the identical delta, so guest-visible absolute
// addresses in code and data stay in step. A module given a different base
// without a relocation table is a hard failure, never a best effort.
bool map_aux_module(const AuxSpec &spec) {
    LoaderModule m;
    m.name = spec.name;
    for (const char *at = spec.aliases; *at;) {
        const char *end = strchr(at, ';');
        end = end ? end : at + strlen(at);
        m.aliases.emplace_back(at, end);
        at = *end ? end + 1 : end;
    }
    m.path = dirname_of(g_exe_path) + "/" + spec.name;
    std::vector<uint8_t> file;
    if (!read_file(m.path.c_str(), file)) {
        const std::string root = std::string(RECOMP_DEVELOPER_GAME_DIR) + "/";
        const std::string configured = spec.path;
        bool found = false;
        if (configured.compare(0, root.size(), root) == 0) {
            m.path = dirname_of(g_exe_path) + "/" + configured.substr(root.size());
            found = read_file(m.path.c_str(), file);
        }
        if (!found) {
            // Modules kept outside the install still have a developer fallback.
            m.path = configured;
            found = read_file(m.path.c_str(), file);
        }
        if (!found) {
            g_error =
                std::string("cannot read auxiliary module ") + spec.name + " beside " + g_exe_path;
            return false;
        }
    }
    std::string digest = sha256_hex(file);
    if (digest != spec.sha256) {
        g_error = m.path + " has SHA-256 " + digest + ", expected " + spec.sha256;
        return false;
    }
    m.sha256 = digest;
    PeLayout pe;
    if (!pe_layout(file, m.path, pe, g_error))
        return false;
    size_t opt_off = pe.opt_off;
    uint16_t opt_size = pe.opt_size, opt_magic = 0x10b;
    uint32_t entry_rva = rd<uint32_t>(file, opt_off + 16);
    uint32_t image_base = rd<uint32_t>(file, opt_off + 28);
    uint32_t size_image = pe.size_image;
    if (size_image > spec.size || spec.base < GUEST_SHIM_END ||
        (uint64_t)spec.base + spec.size > GUEST_SIZE) {
        char buf[200];
        snprintf(
            buf, sizeof buf,
            "%s: [%08x, %08x) does not fit the configured [%08x, %08x) above the arena's shims",
            spec.name, spec.base, spec.base + size_image, spec.base, spec.base + spec.size);
        g_error = buf;
        return false;
    }
    for (const LoaderModule &other : g_aux)
        if (spec.base < other.base + other.size && other.base < spec.base + spec.size) {
            g_error = std::string(spec.name) + " overlaps " + other.name;
            return false;
        }
    m.base = spec.base;
    m.size = spec.size;
    m.entry = entry_rva ? spec.base + entry_rva : 0;
    m.attached = false;
    m.export_rva = rd<uint32_t>(file, opt_off + 96);
    m.export_size = rd<uint32_t>(file, opt_off + 100);
    if (m.export_rva && !rva_in(size_image, m.export_rva, m.export_size)) {
        g_error = std::string(spec.name) + ": export directory lies outside the image";
        return false;
    }
    if (rd<uint32_t>(file, opt_off + 92) > 9 && rd<uint32_t>(file, opt_off + 96 + 9 * 8))
        LOGW("%s has a TLS directory, which auxiliary modules do not get", spec.name);

    if (!pe_copy(file, pe, spec.name, spec.base, &m.sections, g_error))
        return false;
    uint32_t reloc_rva = 0, reloc_size = 0;
    if (opt_size >= 96 + 6 * 8) {
        reloc_rva = rd<uint32_t>(file, opt_off + 96 + 5 * 8);
        reloc_size = rd<uint32_t>(file, opt_off + 96 + 5 * 8 + 4);
    }
    if (!pe_relocate(spec.name, spec.base, image_base, size_image, reloc_rva, reloc_size, g_error))
        return false;
    uint32_t index = (uint32_t)g_aux.size();
    g_aux.push_back(m);
    g_aux_pending.push_back({&spec, std::move(file), opt_off, opt_magic, size_image, index});
    LOGV("mapped auxiliary module %s: base %08x size %08x entry %08x", spec.name, m.base,
         size_image, m.entry);
    return true;
}

// DLL-to-DLL edges from each mapped import descriptor, then bind every IAT in
// dependency order. A cycle cannot occur in a well-formed PE and is reported
// rather than guessed at. The executable is bound by its caller afterwards.
bool bind_aux_imports() {
    size_t n = g_aux_pending.size();
    std::vector<std::vector<int>> deps(n);
    for (size_t i = 0; i < n; ++i) {
        const LoaderModule &m = g_aux[i];
        const AuxPending &p = g_aux_pending[i];
        if (!pe_imports(p.file, p.opt_off, p.opt_magic, m.base, p.size_image, false, false,
                        g_aux_pending[i].imports, g_error)) {
            g_error = m.name + ": " + g_error;
            return false;
        }
        for (const PeImportModule &imported : g_aux_pending[i].imports) {
            std::string dll = normalize_module_name(imported.dll.c_str());
            for (size_t j = 0; j < n; ++j)
                if (i != j && answers_to(g_aux[j], dll))
                    deps[i].push_back((int)j);
        }
    }
    std::vector<bool> done(n, false);
    std::vector<uint32_t> order;
    while (order.size() < n) {
        bool progress = false;
        for (size_t i = 0; i < n; ++i) {
            if (done[i])
                continue;
            bool ready = true;
            for (int d : deps[i])
                if (!done[(size_t)d]) {
                    ready = false;
                    break;
                }
            if (!ready)
                continue;
            done[i] = true;
            order.push_back((uint32_t)i);
            progress = true;
        }
        if (!progress) {
            g_error = "auxiliary module import cycle";
            return false;
        }
    }
    for (uint32_t i : order) {
        AuxPending &p = g_aux_pending[i];
        if (!imports_bind(p.imports, false, g_iat_patched, g_iat_data, g_error)) {
            g_error = g_aux[i].name + ": " + g_error;
            return false;
        }
        g_aux[i].initial_image.assign(g_mem + g_aux[i].base, g_mem + g_aux[i].base + g_aux[i].size);
    }
    g_aux_order = order;
    g_aux_deps = deps;
    return true;
}

} // namespace

bool loader_test_relocate(uint32_t base, uint32_t preferred, uint32_t size, uint32_t reloc_rva,
                          uint32_t reloc_size) {
    return pe_relocate("test", base, preferred, size, reloc_rva, reloc_size, g_error);
}

uint32_t loader_module_count() {
    return (uint32_t)g_aux.size();
}
const LoaderModule *loader_module(uint32_t i) {
    return i < g_aux.size() ? &g_aux[i] : nullptr;
}
LoaderModule *loader_module_named(const char *name) {
    if (!name)
        return nullptr;
    if (g_main.base && os_strcasecmp(g_main.name.c_str(), name) == 0)
        return &g_main;
    for (LoaderModule &m : g_aux)
        if (os_strcasecmp(m.name.c_str(), name) == 0)
            return &m;
    for (LoaderModule &m : g_aux)
        for (const std::string &alias : m.aliases)
            if (os_strcasecmp(alias.c_str(), name) == 0)
                return &m;
    return nullptr;
}
uint32_t loader_call_entry(X86 *c, const LoaderModule &m, uint32_t reason) {
    if (!m.entry)
        return 1;
    if (recomp_module_lookup(m.entry) < 0) {
        fprintf(stderr, "[recomp] fatal: %s has an untranslated DLL entry at %08x (reason %u)\n",
                m.name.c_str(), m.entry, reason);
        abort();
    }
    uint32_t result = guest_call(c, m.entry, m.base, reason, 0);
    LOGV("%s: DLL entry %08x reason %u returned %08x", m.name.c_str(), m.entry, reason, result);
    return result;
}

uint32_t loader_attach_module(X86 *c, const char *name) {
    LoaderModule *m = loader_module_named(name);
    if (!m || m->base == loader_image_base())
        return 0;
    if (m->attached) {
        ++m->load_refs;
        return m->base;
    }
    memcpy(g_mem + m->base, m->initial_image.data(), m->initial_image.size());
    m->thread_notifications_disabled = false;
    m->attached = true;
    m->load_refs = 1;
    if (!loader_call_entry(c, *m, 1)) { // DLL_PROCESS_ATTACH
        loader_call_entry(c, *m, 0);    // failed attach still receives detach
        m->attached = false;
        m->load_refs = 0;
        g_error = m->name + " DllMain(DLL_PROCESS_ATTACH) returned FALSE";
        return 0;
    }
    return m->base;
}

const LoaderModule *loader_module_containing(uint32_t addr) {
    if (g_main.base && addr >= g_main.base && addr - g_main.base < g_main.size)
        return &g_main;
    for (const LoaderModule &m : g_aux)
        if (addr >= m.base && addr < m.base + m.size)
            return &m;
    return nullptr;
}
static std::vector<LoaderModule> g_datafiles;

uint32_t loader_map_datafile(const std::string &path, std::string *why) {
    std::vector<uint8_t> file;
    PeLayout pe;
    LoaderModule m;
    m.path = path;
    m.name = path.substr(path.find_last_of("\\/") + 1);
    m.size = 0;
    if (!read_file(path.c_str(), file)) {
        g_error = "cannot read " + path;
    } else if (pe_layout(file, path, pe, g_error)) {
        m.base = heap_alloc(pe.size_image, true, 0x10000);
        if (!m.base)
            g_error = path + ": no guest memory for its image";
        else if (!pe_copy(file, pe, m.name, m.base, &m.sections, g_error))
            heap_free(m.base);
        else {
            m.size = pe.size_image;
            g_datafiles.push_back(m);
            return m.base;
        }
    }
    if (why)
        *why = g_error;
    return 0;
}

const LoaderModule *loader_datafile(uint32_t base) {
    for (const LoaderModule &m : g_datafiles)
        if (m.base == base)
            return &m;
    return nullptr;
}

bool loader_unmap_datafile(uint32_t base) {
    for (size_t i = 0; i < g_datafiles.size(); ++i)
        if (g_datafiles[i].base == base) {
            heap_free(base);
            g_datafiles.erase(g_datafiles.begin() + i);
            return true;
        }
    return false;
}

bool loader_in_image(uint32_t addr) {
    return (addr >= g_base && addr < g_base + g_size) || loader_module_containing(addr) != nullptr;
}

// A forwarded export stores "DLL.name" or "DLL.#ord" where a code RVA would
// be. Follow it into another mapped module. A system-DLL forwarder has no
// mapped module here; the caller reports the missing export rather than
// guessing at a trampoline.
uint32_t loader_follow_forwarder(const std::string &forwarder) {
    std::string module, name;
    uint32_t ordinal = 0;
    bool by_ordinal = false;
    if (!pe_forwarder(forwarder, module, name, ordinal, by_ordinal))
        return 0;
    const LoaderModule *target = loader_module_named(module.c_str());
    if (!target)
        return 0;
    return by_ordinal ? loader_module_export_ordinal(*target, ordinal)
                      : loader_module_export(*target, name.c_str());
}

// The PE export directory read from guest memory, so a lookup sees the mapped
// bytes. Names are compared case sensitively, as GetProcAddress does.
uint32_t loader_module_export(const LoaderModule &m, const char *name) {
    PeExport result = pe_export_name(m.base, m.size, m.export_rva, m.export_size, name);
    return result.forwarder.empty() ? result.address : loader_follow_forwarder(result.forwarder);
}

uint32_t loader_module_export_ordinal(const LoaderModule &m, uint32_t ordinal) {
    PeExport result = pe_export_ordinal(m.base, m.size, m.export_rva, m.export_size, ordinal);
    return result.forwarder.empty() ? result.address : loader_follow_forwarder(result.forwarder);
}

const char *loader_error() {
    return g_error.c_str();
}
uint32_t loader_image_base() {
    return g_base;
}
uint32_t loader_image_size() {
    return g_size;
}
uint32_t loader_image_limit() {
    return g_base && g_size ? g_base + g_size : 0;
}
uint32_t loader_entry_point() {
    return g_entry;
}
const std::vector<SectionInfo> &loader_sections() {
    return g_sections;
}
const std::string &loader_exe_path() {
    return g_exe_path;
}
const std::string &loader_exe_name() {
    return g_main.name;
}
uint32_t loader_iat_patched() {
    return g_iat_patched;
}
uint32_t loader_iat_data_imports() {
    return g_iat_data;
}
X86 *loader_context() {
    return &g_ctx;
}
const LoaderTls &loader_tls() {
    return g_tls;
}

namespace {

struct MainHeaders {
    std::string digest;
    size_t fh_off, opt_off, sh_off;
    uint16_t opt_size, nsections;
    uint32_t entry_rva, image_base, size_image, size_hdrs;
};

// The content hash is a hard gate: there is no override. Everything after it
// trusts the layout of this exact image at its preferred base.
bool check_game_image(const std::vector<uint8_t> &, const MainHeaders &h) {
    if (h.digest != LOADER_EXPECTED_SHA256) {
        g_error = g_exe_path + " has SHA-256 " + h.digest + ", expected " + LOADER_EXPECTED_SHA256;
        return false;
    }
    char buf[128];
    if (h.image_base != IMAGE_BASE) {
        snprintf(buf, sizeof buf,
                 "image base %08x is not the expected %08x (no relocation support)", h.image_base,
                 IMAGE_BASE);
        g_error = buf;
        return false;
    }
    if ((uint64_t)h.image_base + h.size_image > HEAP_BASE) {
        snprintf(buf, sizeof buf,
                 "image ends at %08x, above the heap arena start %08x; raise [game] heap_base",
                 h.image_base + h.size_image, HEAP_BASE);
        g_error = buf;
        return false;
    }
    if (h.image_base + h.entry_rva != LOADER_EXPECTED_ENTRY)
        LOGW("entry point is %08x, expected %08x", h.image_base + h.entry_rva,
             LOADER_EXPECTED_ENTRY);
    return true;
}

// An arbitrary program has no hash to trust, so it must fit the arena beside
// the runtime's fixed regions and use only startup features the loader honours.
bool check_arbitrary_image(const std::vector<uint8_t> &file, const MainHeaders &h) {
    uint64_t end = (uint64_t)h.image_base + h.size_image;
    auto overlaps = [&](uint32_t start, uint32_t limit) {
        return h.image_base < limit && end > start;
    };
    if (!h.size_image || h.image_base < GUEST_NULL_LIMIT || end > GUEST_SIZE ||
        overlaps(HEAP_BASE, HEAP_LIMIT) || overlaps(FIXED_RUNTIME_BASE, GUEST_SHIM_END)) {
        char buf[160];
        snprintf(buf, sizeof buf,
                 "image overlaps the heap or fixed runtime region (%08x..%08x), or lies outside "
                 "the guest arena",
                 FIXED_RUNTIME_BASE, GUEST_SHIM_END);
        g_error = buf;
        return false;
    }
    if (h.size_hdrs > h.size_image || h.size_hdrs > file.size() || !h.entry_rva ||
        h.entry_rva >= h.size_image) {
        g_error = "invalid image headers or entry point";
        return false;
    }
    uint32_t directories = directory_count(file, h.opt_off);
    if (directories > (h.opt_size - PE32_DIRECTORIES) / 8) {
        g_error = "PE data directories are truncated";
        return false;
    }
    for (uint32_t index : {DIRECTORY_BOUND_IMPORT, DIRECTORY_DELAY_IMPORT, DIRECTORY_CLR}) {
        if (index < directories && rd<uint32_t>(file, h.opt_off + PE32_DIRECTORIES + index * 8)) {
            g_error = "unsupported PE startup directory " + std::to_string(index);
            return false;
        }
    }
    uint16_t subsystem = rd<uint16_t>(file, h.opt_off + 68);
    if (subsystem != 2 && subsystem != 3) {
        g_error = "unsupported PE subsystem " + std::to_string(subsystem);
        return false;
    }
    uint16_t characteristics = rd<uint16_t>(file, h.fh_off + 18);
    if (!(characteristics & 0x0002) || (characteristics & 0x2000)) {
        g_error = "PE image is not an executable program";
        return false;
    }
    for (uint16_t i = 0; i < h.nsections; ++i) {
        size_t s = h.sh_off + 40 * i;
        uint32_t vsize = rd<uint32_t>(file, s + 8);
        uint32_t rva = rd<uint32_t>(file, s + 12);
        uint32_t span = vsize ? vsize : rd<uint32_t>(file, s + 16);
        if ((rd<uint32_t>(file, s + 36) & 0x20000000u) && h.entry_rva >= rva &&
            h.entry_rva - rva < span)
            return true;
    }
    g_error = "entry point is outside an executable section";
    return false;
}

// Map the main image's sections and bind its imports once its policy admits
// the headers. register_shims runs on the fresh arena before any IAT binding.
bool load_main(const char *exe_path, ImagePolicy policy, void (*register_shims)()) {
    g_error.clear();
    g_sections.clear();
    g_aux.clear();
    g_aux_pending.clear();
    g_aux_order.clear();
    g_aux_deps.clear();
    g_aux_startup.clear();
    g_main = LoaderModule{};
    g_iat_patched = 0;
    g_iat_data = 0;
    g_exe_path = exe_path && *exe_path ? exe_path : LOADER_DEFAULT_EXE;

    std::vector<uint8_t> file;
    if (!read_file(g_exe_path.c_str(), file)) {
        g_error = "cannot read " + g_exe_path;
        return false;
    }

    std::string digest = sha256_hex(file);
    // Kept so a mod can key its manifest to the exact image it was built
    // against. It is the digest of what was really mapped, not of what was
    // expected, which is why it is recorded here and not made a constant.
    g_exe_sha = digest;

    PeLayout pe;
    if (!pe_main_layout(file, pe, g_error))
        return false;
    if (pe.machine != 0x014c) {
        g_error = "expected a 32-bit x86 PE image";
        return false;
    }
    size_t fh_off = pe.fh_off, opt_off = pe.opt_off, sh_off = pe.sh_off;
    uint16_t opt_size = pe.opt_size, nsections = pe.nsections, opt_magic = 0x10b;
    uint32_t entry_rva = rd<uint32_t>(file, opt_off + 16);
    uint32_t image_base = rd<uint32_t>(file, opt_off + 28);
    uint32_t size_image = rd<uint32_t>(file, opt_off + 56);
    uint32_t size_hdrs = rd<uint32_t>(file, opt_off + 60);

    MainHeaders headers{digest,    fh_off,    opt_off,    sh_off,     opt_size,
                        nsections, entry_rva, image_base, size_image, size_hdrs};
    bool admitted = policy == ImagePolicy::Game ? check_game_image(file, headers)
                                                : check_arbitrary_image(file, headers);
    if (!admitted)
        return false;

    mem_init();
    g_base = image_base;
    g_size = size_image;
    g_entry = image_base + entry_rva;
    g_main.name = policy == ImagePolicy::Game
                      ? std::string(RECOMP_EXECUTABLE)
                      : g_exe_path.substr(g_exe_path.find_last_of("\\/") + 1);
    g_main.path = g_exe_path;
    g_main.sha256 = digest;
    g_main.base = g_base;
    g_main.size = g_size;
    g_main.entry = g_entry;
    g_main.attached = true; // Its process entry must never run as DllMain.
    if (opt_size >= PE32_DIRECTORIES + 8 && directory_count(file, opt_off) > 0) {
        g_main.export_rva = rd<uint32_t>(file, opt_off + PE32_DIRECTORIES);
        g_main.export_size = rd<uint32_t>(file, opt_off + PE32_DIRECTORIES + 4);
        if (g_main.export_rva && !rva_in(g_size, g_main.export_rva, g_main.export_size)) {
            g_error = "main export directory lies outside the image";
            return false;
        }
    }

    if (!pe_copy(file, pe, "", g_base, &g_sections, g_error))
        return false;

    imports_init();
    win32_init(dirname_of(g_exe_path));
    if (register_shims)
        register_shims();

    // TLS directory. Reserve after win32_init, which clears the slot map, and
    // before IAT binding or any guest code can call TlsAlloc.
    g_tls = LoaderTls{0, 0, 0, 0, 0, 0xffffffffu};
    PeTls tls;
    if (!pe_tls(file, pe, image_base, tls, g_error))
        return false;
    if (tls.index_addr) {
        g_tls = {tls.raw_start, tls.raw_end,   tls.index_addr,
                 tls.callbacks, tls.zero_fill, tls_reserve_slot()};
        if (g_tls.index == 0xffffffffu) {
            g_error = "no TLS slot available for the image";
            return false;
        }
        wr32(g_tls.index_addr, g_tls.index);
    }

    // RECOMP_IMPORT_STATS=1 prints the implemented / not-reached / logging-only
    // classification at exit, which is how the "not reached" column of the
    // coverage table gets filled in from a real run.
    if (recomp_env("IMPORT_STATS")) {
        static bool hooked = false;
        if (!hooked) {
            hooked = true;
            atexit([] { imports_dump_report(stderr); });
        }
    }

    // Map and rebase every auxiliary module before binding any IAT: the
    // executable's Lionhead imports resolve to translated export addresses,
    // not to trampolines. DLL-to-DLL imports are bound in dependency order.
    for (uint32_t i = 0; i < RECOMP_AUX_MODULE_COUNT; ++i)
        if (!map_aux_module(g_aux_specs[i]))
            return false;
    if (!bind_aux_imports())
        return false;

    // Windows attaches a statically imported DLL's DllMain before the exe
    // entry, and resolves the transitive closure of those imports. A module
    // the executable does not statically import is mapped here but attached
    // only when the guest calls LoadLibrary. Mark the startup set so
    // loader_attach_modules runs exactly those and load_library_named owns
    // the rest.
    g_aux_startup.assign(g_aux.size(), false);
    std::vector<PeImportModule> main_imports;
    if (!pe_imports(file, opt_off, opt_magic, g_base, g_size, policy == ImagePolicy::Arbitrary,
                    true, main_imports, g_error))
        return false;
    for (const PeImportModule &imported : main_imports) {
        std::string dll = normalize_module_name(imported.dll.c_str());
        for (size_t i = 0; i < g_aux.size(); ++i)
            if (answers_to(g_aux[i], dll))
                g_aux_startup[i] = true;
    }
    for (bool changed = true; changed;) {
        changed = false;
        for (size_t i = 0; i < g_aux.size(); ++i)
            if (g_aux_startup[i])
                for (int j : g_aux_deps[i])
                    if (!g_aux_startup[(size_t)j]) {
                        g_aux_startup[(size_t)j] = true;
                        changed = true;
                    }
    }
    if (!imports_bind(main_imports, policy == ImagePolicy::Arbitrary, g_iat_patched, g_iat_data,
                      g_error))
        return false;
    // Data imports the CRT reads before any call, now that the IAT holds their
    // guest addresses.
    msvcr90_init();

    loader_init_context(&g_ctx);
    if (g_tls.index != 0xffffffffu && !rd32(TLS_BASE + 4 * g_tls.index)) {
        g_error = "cannot allocate the main thread TLS block";
        return false;
    }
    LOGV("loaded %s: base %08x size %08x entry %08x, %u IAT slots, %u trampolines",
         g_exe_path.c_str(), g_base, g_size, g_entry, g_iat_patched, imports_count());
    return true;
}

} // namespace

bool loader_load(const char *exe_path) {
    return load_main(exe_path, ImagePolicy::Game, nullptr);
}

void loader_register_interpreted_dispatch() {
    g_interpreted_dispatch = true;
}

bool loader_load_interpreted(const char *exe_path, void (*register_shims)()) {
    if (!g_interpreted_dispatch || recomp_func_count || recomp_dispatch.count ||
        recomp_dispatch.span || recomp_module_count() || RECOMP_AUX_MODULE_COUNT) {
        g_error = "interpreted loading requires an empty translated dispatch";
        return false;
    }
    if (!exe_path || !*exe_path) {
        g_error = "no executable path supplied";
        return false;
    }
    return load_main(exe_path, ImagePolicy::Arbitrary, register_shims);
}

void loader_init_context(X86 *c) {
    recomp_seh_reset(c);
    x86_init_context(c);
    c->fs_base = TEB_BASE;

    // TEB: FS:[0] SEH chain head (empty = -1), FS:[4] stack base (high),
    // FS:[8] stack limit (low), FS:[0x18] TEB self pointer, FS:[0x2c] TLS array.
    memset(g_mem + TEB_BASE, 0, TEB_SIZE);
    wr32(TEB_BASE + 0x00, 0xffffffffu);
    wr32(TEB_BASE + 0x04, STACK_TOP);
    wr32(TEB_BASE + 0x08, STACK_LIMIT);
    wr32(TEB_BASE + 0x18, TEB_BASE);
    wr32(TEB_BASE + 0x2c, TLS_BASE);
    wr32(TEB_BASE + 0x30, TEB_BASE - 0x1000); // PEB placeholder (zeroed page)
    memset(g_mem + TLS_BASE, 0, TLS_SLOTS * 4);
    loader_tls_block_for_thread(TLS_BASE);

    // Stack: 16-byte aligned, sentinel return address on top so a RET from the
    // entry point lands somewhere recognisable.
    uint32_t esp = (STACK_TOP - 0x20) & ~0xfu;
    esp -= 4;
    wr32(esp, GUEST_RETURN_SENTINEL);
    c->r[R_ESP] = esp;
    c->r[R_EBP] = 0;
    c->eip = g_entry;
}

const char *loader_exe_sha256() {
    return g_exe_sha.c_str();
}

std::string loader_hash_file(const char *path) {
    std::vector<uint8_t> file;
    if (!path || !read_file(path, file))
        return "";
    return sha256_hex(file);
}

// DllMain of every mapped auxiliary module, in the import-dependency order
// bind_aux_imports established. A FALSE from DLL_PROCESS_ATTACH is a load
// failure: stop with the module named instead of entering the exe with a
// half-initialised DLL. Detach ignores the return value, as Windows does.
void loader_attach_modules(X86 *c) {
    for (uint32_t index : g_aux_order) {
        LoaderModule &m = g_aux[index];
        if (!g_aux_startup[index] || m.attached)
            continue;
        if (!m.entry || recomp_module_lookup(m.entry) < 0) {
            fprintf(stderr,
                    "[recomp] fatal: %s has no translated DllMain at %08x; its exports are not "
                    "callable, so the process cannot start\n",
                    m.name.c_str(), m.entry);
            abort();
        }
        m.attached = true;
        m.load_refs = 1;
        if (!guest_call(c, m.entry, m.base, 1, 0)) {
            fprintf(stderr, "[recomp] fatal: %s DllMain(DLL_PROCESS_ATTACH) returned FALSE\n",
                    m.name.c_str());
            abort();
        }
    }
}

void loader_detach_modules(X86 *c) {
    for (size_t i = g_aux_order.size(); i-- > 0;) {
        LoaderModule &m = g_aux[g_aux_order[i]];
        if (!m.attached)
            continue;
        if (m.entry && recomp_module_lookup(m.entry) >= 0)
            guest_call(c, m.entry, m.base, 0, 0);
        m.attached = false;
        m.load_refs = 0;
    }
}

void run_entry(X86 *c) {
    if (!g_entry) {
        LOGW("run_entry: no image loaded");
        return;
    }
    c->eip = g_entry;
    // This is threads()[0], and it is a guest thread for as long as guest code
    // runs on it. Registered rather than inferred, for the reason in
    // kernel32.cpp: t_self defaults to 0, so inference would make every host
    // thread claim to be this one.
    sched_set_guest_thread(true);
    // ExitProcess/TerminateProcess/ExitThread-outside-a-thread longjmp here.
    if (setjmp(*process_exit_jmp()) == 0) {
        // Static imports are attached before the exe entry point, as Windows
        // does, so a DLL's globals are live before the CRT calls into it.
        loader_attach_modules(c);
        uint32_t count = 0;
        for (uint32_t p = g_tls.callbacks; p && rva_ok(p - g_base, 4); p += 4) {
            uint32_t cb = rd32(p);
            if (!cb)
                break;
            // The guest sees a pushed call frame, never host pointers. Restore
            // ESP explicitly so both caller and callee cleanup leave the entry
            // point's original frame intact.
            uint32_t esp = c->r[R_ESP];
            wr32(esp - 4, 0);
            wr32(esp - 8, 1); // DLL_PROCESS_ATTACH
            wr32(esp - 12, g_base);
            wr32(esp - 16, GUEST_RETURN_SENTINEL);
            c->r[R_ESP] = esp - 16;
            c->eip = cb;
            recomp_run(c, cb);
            c->r[R_ESP] = esp;
            ++count;
        }
        LOGV("ran %u TLS process-attach callbacks", count);
        c->eip = g_entry;
        recomp_run(c, g_entry);
    } else
        LOGW("guest process exited with code %u", process_exit_code());
    loader_detach_modules(c);
    sched_set_guest_thread(false);
}

void run_entry() {
    run_entry(&g_ctx);
}
