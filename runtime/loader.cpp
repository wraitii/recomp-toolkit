#include "seh.h"
#include "loader.h"

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
bool g_interpreted = false;
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
bool patch_iat(const std::vector<uint8_t> &file, size_t opt_off, uint16_t opt_magic,
               uint32_t g_base, uint32_t g_size) {
    auto rva_ok = [g_size](uint32_t rva, uint32_t len) { return rva_in(g_size, rva, len); };
    size_t dd_off = opt_off + (opt_magic == 0x20b ? 112 : 96);
    if (g_interpreted && rd<uint32_t>(file, opt_off + 92) <= 1)
        return true;
    if (dd_off + 16 > file.size()) {
        g_error = "PE data directories are truncated";
        return false;
    }
    uint32_t imp_rva = rd<uint32_t>(file, dd_off + 8 * 1 + 0);
    uint32_t imp_size = rd<uint32_t>(file, dd_off + 8 * 1 + 4);
    if (!imp_rva || !imp_size) {
        if (g_interpreted && bool(imp_rva) != bool(imp_size)) {
            g_error = "incomplete import directory";
            return false;
        }
        if (g_interpreted || g_base != loader_image_base())
            return true;
        g_error = "image has no import directory";
        return false;
    }
    if (!rva_ok(imp_rva, imp_size)) {
        g_error = "import directory lies outside the image";
        return false;
    }

    // One descriptor per DLL, bounded by the directory size.
    uint32_t max_desc = imp_size / 20 + (g_interpreted ? 0 : 1);
    bool terminated = false;
    uint32_t desc_rva = imp_rva;
    for (uint32_t d = 0; d < max_desc; ++d, desc_rva += 20) {
        if (!rva_ok(desc_rva, 20)) {
            g_error = "import descriptor runs past the image";
            return false;
        }
        uint32_t desc = g_base + desc_rva;
        uint32_t orig_thunk = rd32(desc + 0);
        uint32_t name_rva = rd32(desc + 12);
        uint32_t first_thunk = rd32(desc + 16);
        if (!name_rva && !first_thunk && !orig_thunk) {
            terminated = true;
            break;
        }
        if (!rva_ok(name_rva, 1) || !rva_ok(first_thunk, 4)) {
            g_error = "import descriptor points outside the image";
            return false;
        }
        std::string dll = gm_str(g_base + name_rva, 260);

        uint32_t lookup_rva = orig_thunk ? orig_thunk : first_thunk;
        uint32_t slot_rva = first_thunk;
        for (;; lookup_rva += 4, slot_rva += 4) {
            if (!rva_ok(lookup_rva, 4) || !rva_ok(slot_rva, 4)) {
                g_error = "import thunk array runs past the image";
                return false;
            }
            uint32_t lookup = g_base + lookup_rva;
            uint32_t slot = g_base + slot_rva;
            uint32_t t = rd32(lookup);
            if (!t)
                break;
            char namebuf[280];
            if (t & 0x80000000u) {
                snprintf(namebuf, sizeof namebuf, "ord%u", t & 0xffff);
            } else {
                if (!rva_ok(t, 3)) {
                    g_error = "import name lies outside the image";
                    return false;
                }
                std::string n = gm_str(g_base + t + 2, 260);
                snprintf(namebuf, sizeof namebuf, "%s", n.c_str());
            }
            // DLLs can import code AND data from the executable. Both slots
            // must hold the original guest address, not a host trampoline or
            // a separately allocated copy of an exported global.
            if (const LoaderModule *module = loader_module_named(dll.c_str())) {
                if (g_interpreted) {
                    g_error = "unsupported guest module import " + dll;
                    return false;
                }
                uint32_t address = (t & 0x80000000u)
                                       ? loader_module_export_ordinal(*module, t & 0xffff)
                                       : loader_module_export(*module, namebuf);
                if (!address) {
                    g_error = "missing mapped export " + dll + "!" + namebuf;
                    return false;
                }
                wr32(slot, address);
                ++g_iat_patched;
                continue;
            }
            uint32_t data = imports_alloc_data(dll.c_str(), namebuf);
            if (data) {
                // An imported variable, not a function: the slot holds the
                // address of zeroed guest storage.
                wr32(slot, data);
                ++g_iat_patched;
                ++g_iat_data;
                continue;
            }
            // A DLL with a shim table keeps its per-name stubs, including the
            // deliberate logging-only ones. A strict DLL is one whose table is
            // still being filled in: its gaps must abort under their own name,
            // not return zero. A DLL with no table at all is not merely
            // unimplemented either: calling into it must stop with the name of
            // the missing dll!function rather than returning zero.
            uint32_t tramp = g_interpreted && !imports_implemented(dll.c_str(), namebuf)
                                 ? imports_alloc_trampoline(dll.c_str(), namebuf,
                                                            imports_unsupported, ARGC_UNKNOWN)
                                 : imports_resolve(dll.c_str(), namebuf);
            if (!tramp) {
                void (*unknown)(X86 *) =
                    (imports_has_dll(dll.c_str()) && !imports_is_strict_dll(dll.c_str()))
                        ? nullptr
                        : imports_unsupported;
                tramp = imports_alloc_trampoline(dll.c_str(), namebuf, unknown, ARGC_UNKNOWN);
            }
            if (!tramp) {
                g_error = "ran out of import trampolines";
                return false;
            }
            wr32(slot, tramp);
            ++g_iat_patched;
        }
    }
    if (g_interpreted && !terminated) {
        g_error = "import directory lacks a terminating descriptor";
        return false;
    }
    return true;
}

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
bool relocate_aux_module(const char *name, uint32_t base, uint32_t preferred, uint32_t size,
                         uint32_t reloc_rva, uint32_t reloc_size) {
    if (base == preferred)
        return true;
    if (!reloc_rva || !reloc_size) {
        g_error =
            std::string(name) + ": configured base needs a base relocation table, and it has none";
        return false;
    }
    int32_t delta = (int32_t)(base - preferred);
    uint32_t off = reloc_rva, end = reloc_rva + reloc_size;
    while (off + 8 <= end) {
        if (!rva_in(size, off, 8)) {
            g_error = std::string(name) + ": base relocation block header lies outside the image";
            return false;
        }
        uint32_t page = rd32(base + off);
        uint32_t blk = rd32(base + off + 4);
        if (blk < 8 || off + blk > end || !rva_in(size, off, blk)) {
            g_error = std::string(name) + ": malformed base relocation block";
            return false;
        }
        for (uint32_t k = off + 8; k + 2 <= off + blk; k += 2) {
            uint16_t e = rd16(base + k);
            if ((e >> 12) != 3) // IMAGE_REL_BASED_HIGHLOW; ABSOLUTE padding names nothing
                continue;
            uint32_t site = page + (e & 0xfff);
            if (!rva_in(size, site, 4)) {
                g_error = std::string(name) + ": base relocation site lies outside the image";
                return false;
            }
            wr32(base + site, rd32(base + site) + (uint32_t)delta);
        }
        off += blk;
    }
    return true;
}

struct PeLayout {
    size_t opt_off = 0, sh_off = 0;
    uint16_t opt_size = 0, nsections = 0;
    uint32_t size_image = 0, size_hdrs = 0;
};

bool pe_layout(const std::vector<uint8_t> &file, const std::string &path, PeLayout &pe) {
    if (file.size() < 0x40 || rd<uint16_t>(file, 0) != 0x5a4d) {
        g_error = path + " is not an MZ image";
        return false;
    }
    uint32_t pe_off = rd<uint32_t>(file, 0x3c);
    if ((uint64_t)pe_off + 24 > file.size() || rd<uint32_t>(file, pe_off) != 0x00004550) {
        g_error = path + " is not a PE image";
        return false;
    }
    size_t fh_off = pe_off + 4;
    pe.nsections = rd<uint16_t>(file, fh_off + 2);
    pe.opt_size = rd<uint16_t>(file, fh_off + 16);
    pe.opt_off = fh_off + 20;
    if (pe.opt_off + 96 > file.size() || rd<uint16_t>(file, pe.opt_off) != 0x10b ||
        pe.opt_size < 96) {
        g_error = path + " is not a PE32 image";
        return false;
    }
    pe.sh_off = pe.opt_off + pe.opt_size;
    if (pe.nsections == 0 || pe.sh_off + 40ull * pe.nsections > file.size()) {
        g_error = path + " has a truncated section table";
        return false;
    }
    pe.size_image = rd<uint32_t>(file, pe.opt_off + 56);
    pe.size_hdrs = rd<uint32_t>(file, pe.opt_off + 60);
    return true;
}

bool pe_copy(const std::vector<uint8_t> &file, const PeLayout &pe, const std::string &name,
             uint32_t base, std::vector<SectionInfo> *sections) {
    size_t hdr_copy = std::min<size_t>(std::min(pe.size_hdrs, pe.size_image), file.size());
    memcpy(g_mem + base, file.data(), hdr_copy);
    for (uint16_t i = 0; i < pe.nsections; ++i) {
        size_t s = pe.sh_off + 40 * i;
        char nm[9] = {0};
        memcpy(nm, file.data() + s, 8);
        uint32_t vsize = rd<uint32_t>(file, s + 8);
        uint32_t va = base + rd<uint32_t>(file, s + 12);
        uint32_t raw_size = rd<uint32_t>(file, s + 16);
        uint32_t raw_ptr = rd<uint32_t>(file, s + 20);
        uint32_t span = vsize ? vsize : raw_size;
        sections->push_back({nm, va, vsize, raw_size, raw_ptr, rd<uint32_t>(file, s + 36)});
        if (va < base || (uint64_t)va + span > (uint64_t)base + pe.size_image) {
            g_error = name + ": section " + nm + " lies outside the image";
            return false;
        }
        uint32_t copy = raw_size;
        if (vsize && copy > vsize)
            copy = vsize;
        if (raw_ptr && copy) {
            if ((uint64_t)raw_ptr + copy > file.size()) {
                g_error = name + ": section " + nm + " raw data is truncated";
                return false;
            }
            memcpy(g_mem + va, file.data() + raw_ptr, copy);
        }
    }
    return true;
}

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
    if (!pe_layout(file, m.path, pe))
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

    if (!pe_copy(file, pe, spec.name, spec.base, &m.sections))
        return false;
    uint32_t reloc_rva = 0, reloc_size = 0;
    if (opt_size >= 96 + 6 * 8) {
        reloc_rva = rd<uint32_t>(file, opt_off + 96 + 5 * 8);
        reloc_size = rd<uint32_t>(file, opt_off + 96 + 5 * 8 + 4);
    }
    if (!relocate_aux_module(spec.name, spec.base, image_base, size_image, reloc_rva, reloc_size))
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
        uint32_t imp_rva = rd<uint32_t>(p.file, p.opt_off + 96 + 1 * 8);
        uint32_t imp_size = rd<uint32_t>(p.file, p.opt_off + 96 + 1 * 8 + 4);
        if (!imp_rva || !imp_size || !rva_in(p.size_image, imp_rva, imp_size))
            continue;
        uint32_t max_desc = imp_size / 20 + 1;
        for (uint32_t d = 0; d < max_desc; ++d) {
            uint32_t desc_rva = imp_rva + d * 20;
            if (!rva_in(p.size_image, desc_rva, 20))
                break;
            uint32_t name_rva = rd32(m.base + desc_rva + 12);
            uint32_t first = rd32(m.base + desc_rva + 16);
            uint32_t orig = rd32(m.base + desc_rva + 0);
            if (!name_rva && !first && !orig)
                break;
            if (!rva_in(p.size_image, name_rva, 1))
                break;
            std::string dll = normalize_module_name(gm_str(m.base + name_rva, 260).c_str());
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
        if (!patch_iat(p.file, p.opt_off, p.opt_magic, g_aux[i].base, p.size_image)) {
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
    return relocate_aux_module("test", base, preferred, size, reloc_rva, reloc_size);
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
    } else if (pe_layout(file, path, pe)) {
        m.base = heap_alloc(pe.size_image, true, 0x10000);
        if (!m.base)
            g_error = path + ": no guest memory for its image";
        else if (!pe_copy(file, pe, m.name, m.base, &m.sections))
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
uint32_t loader_follow_forwarder(const LoaderModule &m, uint32_t rva) {
    std::string s = gm_str(m.base + rva, std::min(260u, m.size - rva));
    size_t dot = s.find('.');
    if (dot == std::string::npos || dot == 0 || dot + 1 >= s.size())
        return 0;
    const LoaderModule *target = loader_module_named(s.substr(0, dot).c_str());
    if (!target)
        return 0;
    std::string fn = s.substr(dot + 1);
    if (!fn.empty() && fn[0] == '#') {
        char *end = nullptr;
        long ord = strtol(fn.c_str() + 1, &end, 10);
        return end && *end == '\0' ? loader_module_export_ordinal(*target, (uint32_t)ord) : 0;
    }
    return loader_module_export(*target, fn.c_str());
}

// The PE export directory read from guest memory, so a lookup sees the mapped
// bytes. Names are compared case sensitively, as GetProcAddress does.
uint32_t loader_module_export(const LoaderModule &m, const char *name) {
    if (!m.export_rva || !name || !rva_in(m.size, m.export_rva, 40))
        return 0;
    auto in = [&](uint32_t rva, uint32_t len) { return rva_in(m.size, rva, len); };
    uint32_t dir = m.base + m.export_rva;
    uint32_t nfuncs = rd32(dir + 20), nnames = rd32(dir + 24);
    uint32_t funcs = rd32(dir + 28), names = rd32(dir + 32), ords = rd32(dir + 36);
    if (nfuncs > m.size / 4 || nnames > m.size / 4 || !in(funcs, 4 * nfuncs) ||
        !in(names, 4 * nnames) || !in(ords, 2 * nnames))
        return 0;
    const size_t requested_length = strlen(name);
    for (uint32_t i = 0; i < nnames; ++i) {
        uint32_t name_rva = rd32(m.base + names + 4 * i);
        if (!in(name_rva, 1))
            continue;
        const uint32_t name_addr = m.base + name_rva;
        size_t available = 0;
        if (name_addr != 0 && name_addr < GUEST_SIZE)
            available = std::min((size_t)(m.size - name_rva), (size_t)(GUEST_SIZE - name_addr));
        if (available) {
            const char *export_name = (const char *)(g_mem + name_addr);
            if (requested_length > available ||
                (requested_length && memcmp(export_name, name, requested_length) != 0) ||
                (requested_length < available && export_name[requested_length] != 0))
                continue;
        } else if (requested_length != 0) {
            continue;
        }
        uint32_t ordinal = rd16(m.base + ords + 2 * i);
        if (ordinal >= nfuncs)
            return 0;
        uint32_t rva = rd32(m.base + funcs + 4 * ordinal);
        if (!rva || !in(rva, 1))
            return 0;
        // A forwarder (an RVA inside the export directory) names another export.
        if (rva_in(m.export_size, rva - m.export_rva, 1))
            return loader_follow_forwarder(m, rva);
        return m.base + rva;
    }
    return 0;
}

// Ordinals index the same address table as names, offset by the directory's
// ordinal base. Missing entries and unresolved forwards are not executable.
uint32_t loader_module_export_ordinal(const LoaderModule &m, uint32_t ordinal) {
    if (!m.export_rva || !rva_in(m.size, m.export_rva, 40))
        return 0;
    uint32_t dir = m.base + m.export_rva;
    uint32_t first = rd32(dir + 16), count = rd32(dir + 20), table = rd32(dir + 28);
    if (ordinal < first || ordinal - first >= count || count > m.size / 4 ||
        !rva_in(m.size, table, 4 * count))
        return 0;
    uint32_t rva = rd32(m.base + table + 4 * (ordinal - first));
    if (!rva || !rva_in(m.size, rva, 1))
        return 0;
    if (rva_in(m.export_size, rva - m.export_rva, 1))
        return loader_follow_forwarder(m, rva);
    return m.base + rva;
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

// Verify the supported executable hash, map PE sections and bind its imports.
// All later address-based translation assumes this exact image; a mismatch is a hard failure.
static bool loader_load_impl(const char *exe_path, bool interpreted) {
    g_interpreted = interpreted;
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

    // The content hash is a hard gate: there is no override. Everything below
    // trusts the layout of this exact image.
    std::string digest = sha256_hex(file);
    // Kept so a mod can key its manifest to the exact image it was built
    // against. It is the digest of what was really mapped, not of what was
    // expected, which is why it is recorded here and not made a constant.
    g_exe_sha = digest;
    if (!interpreted && digest != LOADER_EXPECTED_SHA256) {
        g_error = g_exe_path + " has SHA-256 " + digest + ", expected " + LOADER_EXPECTED_SHA256;
        return false;
    }

    if (file.size() < 0x40 || rd<uint16_t>(file, 0) != 0x5a4d) {
        g_error = "not an MZ image";
        return false;
    }
    uint32_t pe_off = rd<uint32_t>(file, 0x3c);
    if ((uint64_t)pe_off + 24 > file.size() || rd<uint32_t>(file, pe_off) != 0x00004550) {
        g_error = "not a PE image";
        return false;
    }

    size_t fh_off = pe_off + 4;
    uint16_t machine = rd<uint16_t>(file, fh_off + 0);
    uint16_t nsections = rd<uint16_t>(file, fh_off + 2);
    uint16_t opt_size = rd<uint16_t>(file, fh_off + 16);
    size_t opt_off = fh_off + 20;
    if (opt_off + opt_size > file.size() || opt_size < 96) {
        g_error = "PE optional header is truncated";
        return false;
    }
    uint16_t opt_magic = rd<uint16_t>(file, opt_off);
    if (machine != 0x014c || opt_magic != 0x10b) {
        g_error = "expected a 32-bit x86 PE image";
        return false;
    }
    size_t sh_off = opt_off + opt_size;
    if (nsections == 0 || sh_off + 40ull * nsections > file.size()) {
        g_error = "PE section headers are truncated";
        return false;
    }

    uint32_t entry_rva = rd<uint32_t>(file, opt_off + 16);
    uint32_t image_base = rd<uint32_t>(file, opt_off + 28);
    uint32_t size_image = rd<uint32_t>(file, opt_off + 56);
    uint32_t size_hdrs = rd<uint32_t>(file, opt_off + 60);

    if (!interpreted && image_base != IMAGE_BASE) {
        char buf[128];
        snprintf(buf, sizeof buf,
                 "image base %08x is not the expected %08x (no relocation support)", image_base,
                 IMAGE_BASE);
        g_error = buf;
        return false;
    }
    if (interpreted) {
        uint64_t end = (uint64_t)image_base + size_image;
        auto overlaps = [&](uint32_t start, uint32_t limit) {
            return image_base < limit && end > start;
        };
        if (!size_image || image_base < GUEST_NULL_LIMIT || end > GUEST_SIZE ||
            overlaps(HEAP_BASE, HEAP_LIMIT) || overlaps(0x0e000000u, GUEST_SHIM_END)) {
            g_error = "image overlaps the heap or fixed runtime region (0x0e000000..0x10000000), "
                      "or lies outside the guest arena";
            return false;
        }
        if (size_hdrs > size_image || size_hdrs > file.size() || !entry_rva ||
            entry_rva >= size_image) {
            g_error = "invalid image headers or entry point";
            return false;
        }
        uint32_t directories = rd<uint32_t>(file, opt_off + 92);
        if (directories > (opt_size - 96) / 8) {
            g_error = "PE data directories are truncated";
            return false;
        }
        for (uint32_t index : {11u, 13u, 14u}) {
            if (index < directories && rd<uint32_t>(file, opt_off + 96 + index * 8)) {
                g_error = "unsupported PE startup directory " + std::to_string(index);
                return false;
            }
        }
        uint16_t subsystem = rd<uint16_t>(file, opt_off + 68);
        if (subsystem != 2 && subsystem != 3) {
            g_error = "unsupported PE subsystem " + std::to_string(subsystem);
            return false;
        }
        uint16_t characteristics = rd<uint16_t>(file, fh_off + 18);
        if (!(characteristics & 0x0002) || (characteristics & 0x2000)) {
            g_error = "PE image is not an executable program";
            return false;
        }
    }
    if (!interpreted && (uint64_t)image_base + size_image > HEAP_BASE) {
        char buf[128];
        snprintf(buf, sizeof buf,
                 "image ends at %08x, above the heap arena start %08x; raise [game] heap_base",
                 image_base + size_image, HEAP_BASE);
        g_error = buf;
        return false;
    }

    mem_init();
    g_base = image_base;
    g_size = size_image;
    g_entry = image_base + entry_rva;
    g_main.name =
        interpreted ? g_exe_path.substr(g_exe_path.find_last_of("\\/") + 1) : RECOMP_EXECUTABLE;
    g_main.path = g_exe_path;
    g_main.sha256 = digest;
    g_main.base = g_base;
    g_main.size = g_size;
    g_main.entry = g_entry;
    g_main.attached = true; // Its process entry must never run as DllMain.
    if (opt_size >= 104 && rd<uint32_t>(file, opt_off + 92) > 0) {
        g_main.export_rva = rd<uint32_t>(file, opt_off + 96);
        g_main.export_size = rd<uint32_t>(file, opt_off + 100);
        if (g_main.export_rva && !rva_in(g_size, g_main.export_rva, g_main.export_size)) {
            g_error = "main export directory lies outside the image";
            return false;
        }
    }

    // PE headers, then each section at its virtual address. The arena is zero
    // filled by mmap, so the tail of a section past SizeOfRawData (.bss) is
    // already zero.
    size_t hdr_copy = size_hdrs < file.size() ? size_hdrs : file.size();
    memcpy(g_mem + g_base, file.data(), hdr_copy);

    for (uint16_t i = 0; i < nsections; ++i) {
        size_t s = sh_off + 40 * i;
        char nm[9] = {0};
        memcpy(nm, file.data() + s, 8);
        SectionInfo si;
        si.name = nm;
        si.vsize = rd<uint32_t>(file, s + 8);
        si.va = image_base + rd<uint32_t>(file, s + 12);
        si.raw_size = rd<uint32_t>(file, s + 16);
        si.raw_ptr = rd<uint32_t>(file, s + 20);
        si.characteristics = rd<uint32_t>(file, s + 36);

        uint32_t span = si.vsize ? si.vsize : si.raw_size;
        if (si.va < image_base || (uint64_t)si.va + span > (uint64_t)image_base + size_image) {
            g_error = "section " + si.name + " lies outside the image";
            return false;
        }
        uint32_t copy = si.raw_size;
        if (si.vsize && copy > si.vsize)
            copy = si.vsize;
        if (si.raw_ptr && copy) {
            if ((uint64_t)si.raw_ptr + copy > file.size()) {
                g_error = "section " + si.name + " raw data is truncated";
                return false;
            }
            memcpy(g_mem + si.va, file.data() + si.raw_ptr, copy);
        }
        g_sections.push_back(si);
    }

    if (interpreted) {
        bool executable_entry = false;
        for (const auto &section : g_sections) {
            uint32_t span = section.vsize ? section.vsize : section.raw_size;
            if ((section.characteristics & 0x20000000u) && g_entry >= section.va &&
                g_entry - section.va < span)
                executable_entry = true;
        }
        if (!executable_entry) {
            g_error = "entry point is outside an executable section";
            return false;
        }
    }
    imports_init();
    win32_init(dirname_of(g_exe_path));

    // TLS directory (data directory 9). Reserve after win32_init, which clears
    // the slot map, and before IAT binding or any guest code can call TlsAlloc.
    g_tls = LoaderTls{0, 0, 0, 0, 0, 0xffffffffu};
    if (rd<uint32_t>(file, opt_off + 92) > 9) {
        size_t dd_tls = opt_off + 96 + 9 * 8;
        if (opt_size < 96 + 10 * 8) {
            g_error = "PE TLS data directory is truncated";
            return false;
        }
        uint32_t tls_rva = rd<uint32_t>(file, dd_tls);
        uint32_t tls_size = rd<uint32_t>(file, dd_tls + 4);
        if (tls_rva) {
            if (tls_size < 24 || !rva_ok(tls_rva, tls_size)) {
                g_error = "TLS directory is truncated or lies outside the image";
                return false;
            }
            uint32_t d = image_base + tls_rva;
            g_tls.raw_start = rd32(d + 0);
            g_tls.raw_end = rd32(d + 4);
            g_tls.index_addr = rd32(d + 8);
            g_tls.callbacks = rd32(d + 12);
            g_tls.zero_fill = rd32(d + 16);
            if (g_tls.raw_end < g_tls.raw_start ||
                (g_tls.raw_end > g_tls.raw_start &&
                 !rva_ok(g_tls.raw_start - image_base, g_tls.raw_end - g_tls.raw_start)) ||
                !rva_ok(g_tls.index_addr - image_base, 4) ||
                (g_tls.callbacks && !rva_ok(g_tls.callbacks - image_base, 4))) {
                g_error = "TLS directory points outside the image";
                return false;
            }
            g_tls.index = tls_reserve_slot();
            if (g_tls.index == 0xffffffffu) {
                g_error = "no TLS slot available for the image";
                return false;
            }
            wr32(g_tls.index_addr, g_tls.index);
        }
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
    for (uint32_t i = 0; !interpreted && i < RECOMP_AUX_MODULE_COUNT; ++i)
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
    {
        size_t dd = opt_off + 96;
        uint32_t imp_rva = rd<uint32_t>(file, dd + 8);
        uint32_t imp_size = rd<uint32_t>(file, dd + 12);
        if ((!interpreted || rd<uint32_t>(file, opt_off + 92) > 1) && imp_rva && imp_size &&
            rva_ok(imp_rva, imp_size)) {
            for (uint32_t d = 0; d < imp_size / 20 + 1; ++d) {
                uint32_t desc = imp_rva + d * 20;
                if (!rva_ok(desc, 20))
                    break;
                uint32_t name_rva = rd32(g_base + desc + 12);
                uint32_t first = rd32(g_base + desc + 16);
                uint32_t orig = rd32(g_base + desc);
                if (!name_rva && !first && !orig)
                    break;
                if (!rva_ok(name_rva, 1))
                    break;
                std::string dll = normalize_module_name(gm_str(g_base + name_rva, 260).c_str());
                for (size_t i = 0; i < g_aux.size(); ++i)
                    if (answers_to(g_aux[i], dll))
                        g_aux_startup[i] = true;
            }
        }
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
    if (!patch_iat(file, opt_off, opt_magic, g_base, g_size))
        return false;
    // Data imports the CRT reads before any call, now that the IAT holds their
    // guest addresses.
    msvcr90_init();

    if (!interpreted && g_entry != LOADER_EXPECTED_ENTRY)
        LOGW("entry point is %08x, expected %08x", g_entry, LOADER_EXPECTED_ENTRY);

    loader_init_context(&g_ctx);
    if (g_tls.index != 0xffffffffu && !rd32(TLS_BASE + 4 * g_tls.index)) {
        g_error = "cannot allocate the main thread TLS block";
        return false;
    }
    LOGV("loaded %s: base %08x size %08x entry %08x, %u IAT slots, %u trampolines",
         g_exe_path.c_str(), g_base, g_size, g_entry, g_iat_patched, imports_count());
    return true;
}

bool loader_load(const char *exe_path) {
    return loader_load_impl(exe_path, false);
}

extern "C" __attribute__((weak)) int recomp_interpreter_dispatch_ready() {
    return 0;
}

bool loader_load_interpreted(const char *exe_path) {
    if (!recomp_interpreter_dispatch_ready() || recomp_func_count || recomp_dispatch.count ||
        recomp_dispatch.span || recomp_module_count()) {
        g_error = "interpreted loading requires an empty translated dispatch";
        return false;
    }
    if (!exe_path || !*exe_path) {
        g_error = "no executable path supplied";
        return false;
    }
    return loader_load_impl(exe_path, true);
}

void loader_init_context(X86 *c) {
    recomp_seh_reset(c);
    memset(c, 0, sizeof(*c));
    // Process-start CPU state. The x87 values are the post-FINIT ones the CRT
    // assumes: control word 0x037f (round to nearest, 64-bit precision, all
    // exceptions masked), clear status word, all-empty tag word. __ftol,
    // __ctrlfp and __statfp diverge immediately if these are wrong.
    c->fpu_cw = 0x037f;
    c->fpu_sw = 0;
    c->fpu_tag = 0xffff;
    c->fpu_top = 0;
    c->mxcsr = 0x1f80u;
    // EFLAGS with only the reserved bit 1 and IF set, so PUSHFD reads 0x202.
    c->eflags_misc = 0x00000202u;
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
