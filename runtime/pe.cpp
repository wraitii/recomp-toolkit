#include "pe.h"
#include "guest.h"
#include "../platform/os.h"
#include <algorithm>
#include <cstring>
#include <cstdlib>

namespace {
template <typename T> T rd(const std::vector<uint8_t> &d, size_t off) {
    T v{};
    if (off + sizeof(T) <= d.size())
        memcpy(&v, d.data() + off, sizeof(T));
    return v;
}
bool rva_in(uint32_t size, uint32_t rva, uint32_t len) {
    return rva < size && len <= size - rva;
}
} // namespace

bool pe_relocate(const char *name, uint32_t base, uint32_t preferred, uint32_t size,
                 uint32_t reloc_rva, uint32_t reloc_size, std::string &g_error) {
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

bool pe_layout(const std::vector<uint8_t> &file, const std::string &path, PeLayout &pe,
               std::string &g_error) {
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
             uint32_t base, std::vector<SectionInfo> *sections, std::string &g_error) {
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
            g_error =
                (name.empty() ? "" : name + ": ") + "section " + nm + " lies outside the image";
            return false;
        }
        uint32_t copy = raw_size;
        if (vsize && copy > vsize)
            copy = vsize;
        if (raw_ptr && copy) {
            if ((uint64_t)raw_ptr + copy > file.size()) {
                g_error =
                    (name.empty() ? "" : name + ": ") + "section " + nm + " raw data is truncated";
                return false;
            }
            memcpy(g_mem + va, file.data() + raw_ptr, copy);
        }
    }
    return true;
}

bool pe_imports(const std::vector<uint8_t> &file, size_t opt_off, uint16_t opt_magic,
                uint32_t g_base, uint32_t g_size, bool strict, bool require_directory,
                std::vector<PeImportModule> &modules, std::string &g_error) {
    auto rva_ok = [g_size](uint32_t rva, uint32_t len) { return rva_in(g_size, rva, len); };
    size_t dd_off = opt_off + (opt_magic == 0x20b ? 112 : 96);
    if (strict && rd<uint32_t>(file, opt_off + 92) <= 1)
        return true;
    if (dd_off + 16 > file.size()) {
        g_error = "PE data directories are truncated";
        return false;
    }
    uint32_t imp_rva = rd<uint32_t>(file, dd_off + 8 * 1 + 0);
    uint32_t imp_size = rd<uint32_t>(file, dd_off + 8 * 1 + 4);
    if (!imp_rva || !imp_size) {
        if (strict && bool(imp_rva) != bool(imp_size)) {
            g_error = "incomplete import directory";
            return false;
        }
        if (strict || !require_directory)
            return true;
        g_error = "image has no import directory";
        return false;
    }
    if (!rva_ok(imp_rva, imp_size)) {
        g_error = "import directory lies outside the image";
        return false;
    }

    // One descriptor per DLL, bounded by the directory size.
    uint32_t max_desc = imp_size / 20 + (strict ? 0 : 1);
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
        modules.push_back({gm_str(g_base + name_rva, 260), {}});
        PeImportModule &module = modules.back();

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
            module.imports.push_back({slot, 4, t & 0xffffu, namebuf, (t & 0x80000000u) != 0});
        }
    }
    if (strict && !terminated) {
        g_error = "import directory lacks a terminating descriptor";
        return false;
    }
    return true;
}

PeExport pe_export_name(uint32_t base, uint32_t size, uint32_t export_rva, uint32_t export_size,
                        const char *name) {
    if (!export_rva || !name || !rva_in(size, export_rva, 40))
        return {};
    auto in = [&](uint32_t rva, uint32_t len) { return rva_in(size, rva, len); };
    uint32_t dir = base + export_rva;
    uint32_t nfuncs = rd32(dir + 20), nnames = rd32(dir + 24);
    uint32_t funcs = rd32(dir + 28), names = rd32(dir + 32), ords = rd32(dir + 36);
    if (nfuncs > size / 4 || nnames > size / 4 || !in(funcs, 4 * nfuncs) ||
        !in(names, 4 * nnames) || !in(ords, 2 * nnames))
        return {};
    const size_t requested_length = strlen(name);
    for (uint32_t i = 0; i < nnames; ++i) {
        uint32_t name_rva = rd32(base + names + 4 * i);
        if (!in(name_rva, 1))
            continue;
        const uint32_t name_addr = base + name_rva;
        size_t available = 0;
        if (name_addr != 0 && name_addr < GUEST_SIZE)
            available = std::min((size_t)(size - name_rva), (size_t)(GUEST_SIZE - name_addr));
        if (available) {
            const char *export_name = (const char *)(g_mem + name_addr);
            if (requested_length > available ||
                (requested_length && memcmp(export_name, name, requested_length) != 0) ||
                (requested_length < available && export_name[requested_length] != 0))
                continue;
        } else if (requested_length != 0) {
            continue;
        }
        uint32_t ordinal = rd16(base + ords + 2 * i);
        if (ordinal >= nfuncs)
            return {};
        uint32_t rva = rd32(base + funcs + 4 * ordinal);
        if (!rva || !in(rva, 1))
            return {};
        // A forwarder (an RVA inside the export directory) names another export.
        if (rva_in(export_size, rva - export_rva, 1))
            return {0, gm_str(base + rva, std::min(260u, size - rva))};
        return {base + rva, {}};
    }
    return {};
}

// Ordinals index the same address table as names, offset by the directory's
// ordinal base. Missing entries and unresolved forwards are not executable.
PeExport pe_export_ordinal(uint32_t base, uint32_t size, uint32_t export_rva, uint32_t export_size,
                           uint32_t ordinal) {
    if (!export_rva || !rva_in(size, export_rva, 40))
        return {};
    uint32_t dir = base + export_rva;
    uint32_t first = rd32(dir + 16), count = rd32(dir + 20), table = rd32(dir + 28);
    if (ordinal < first || ordinal - first >= count || count > size / 4 ||
        !rva_in(size, table, 4 * count))
        return {};
    uint32_t rva = rd32(base + table + 4 * (ordinal - first));
    if (!rva || !rva_in(size, rva, 1))
        return {};
    if (rva_in(export_size, rva - export_rva, 1))
        return {0, gm_str(base + rva, std::min(260u, size - rva))};
    return {base + rva, {}};
}

bool pe_main_layout(const std::vector<uint8_t> &file, PeLayout &pe, std::string &error) {
    if (file.size() < 0x40 || rd<uint16_t>(file, 0) != 0x5a4d) {
        error = "not an MZ image";
        return false;
    }
    uint32_t pe_off = rd<uint32_t>(file, 0x3c);
    if ((uint64_t)pe_off + 24 > file.size() || rd<uint32_t>(file, pe_off) != 0x00004550) {
        error = "not a PE image";
        return false;
    }

    size_t fh_off = pe_off + 4;
    pe.fh_off = fh_off;
    pe.machine = rd<uint16_t>(file, fh_off + 0);
    uint16_t nsections = rd<uint16_t>(file, fh_off + 2);
    uint16_t opt_size = rd<uint16_t>(file, fh_off + 16);
    size_t opt_off = fh_off + 20;
    pe.opt_off = opt_off;
    pe.opt_size = opt_size;
    pe.nsections = nsections;
    if (opt_off + opt_size > file.size() || opt_size < 96) {
        error = "PE optional header is truncated";
        return false;
    }
    pe.magic = rd<uint16_t>(file, opt_off);
    if (pe.magic != 0x10b) {
        error = "expected a 32-bit x86 PE image";
        return false;
    }
    size_t sh_off = opt_off + opt_size;
    pe.sh_off = sh_off;
    if (nsections == 0 || sh_off + 40ull * nsections > file.size()) {
        error = "PE section headers are truncated";
        return false;
    }

    pe.size_image = rd<uint32_t>(file, opt_off + 56);
    pe.size_hdrs = rd<uint32_t>(file, opt_off + 60);
    return true;
}

bool pe_tls(const std::vector<uint8_t> &file, const PeLayout &pe, uint32_t image_base, PeTls &tls,
            std::string &error) {
    size_t opt_off = pe.opt_off;
    uint16_t opt_size = pe.opt_size;
    auto in_image = [&](uint32_t rva, uint32_t size) { return rva_in(pe.size_image, rva, size); };
    if (rd<uint32_t>(file, opt_off + 92) > 9) {
        size_t dd_tls = opt_off + 96 + 9 * 8;
        if (opt_size < 96 + (9 + 1) * 8) {
            error = "PE TLS data directory is truncated";
            return false;
        }
        uint32_t tls_rva = rd<uint32_t>(file, dd_tls);
        uint32_t tls_size = rd<uint32_t>(file, dd_tls + 4);
        if (tls_rva) {
            if (tls_size < 24 || !in_image(tls_rva, tls_size)) {
                error = "TLS directory is truncated or lies outside the image";
                return false;
            }
            uint32_t d = image_base + tls_rva;
            tls.raw_start = rd32(d + 0);
            tls.raw_end = rd32(d + 4);
            tls.index_addr = rd32(d + 8);
            tls.callbacks = rd32(d + 12);
            tls.zero_fill = rd32(d + 16);
            if (tls.raw_end < tls.raw_start ||
                (tls.raw_end > tls.raw_start &&
                 !in_image(tls.raw_start - image_base, tls.raw_end - tls.raw_start)) ||
                !in_image(tls.index_addr - image_base, 4) ||
                (tls.callbacks && !in_image(tls.callbacks - image_base, 4))) {
                error = "TLS directory points outside the image";
                return false;
            }
        }
    }

    return true;
}

bool pe_directory(uint32_t base, uint32_t image_size, uint32_t index, uint32_t &rva,
                  uint32_t &size) {
    rva = size = 0;
    if (!base || !gm_valid(base, image_size) || image_size < 64 || rd16(base) != 0x5a4d)
        return false;
    uint32_t nt = rd32(base + 0x3c);
    if (!rva_in(image_size, nt, 24) || rd32(base + nt) != 0x4550)
        return false;
    uint32_t opt = nt + 24, opt_size = rd16(base + nt + 20);
    if (index > (UINT32_MAX - 104) / 8 || opt_size < 96 + (index + 1) * 8 ||
        !rva_in(image_size, opt, opt_size) || rd16(base + opt) != 0x10b ||
        rd32(base + opt + 92) <= index)
        return false;
    rva = rd32(base + opt + 96 + index * 8);
    size = rd32(base + opt + 100 + index * 8);
    return true;
}

namespace {
bool fail(std::string *why, const char *message) {
    if (why)
        *why = message;
    return false;
}
// Every directory-relative offset, count and payload RVA is checked before a
// guest read. PE resource strings carry a WORD length, not a NUL terminator.
struct Directory {
    uint32_t image = 0, image_size = 0, root = 0, size = 0;
    std::string *why;
    explicit Directory(std::string *reason) : why(reason) {
        if (why)
            why->clear();
    }
    bool image_range(uint32_t rva, uint32_t bytes) const {
        return rva <= image_size && bytes <= image_size - rva;
    }
    bool relative(uint32_t offset, uint32_t bytes) const {
        return offset <= size && bytes <= size - offset;
    }
    bool open(uint32_t base, uint32_t length) {
        image = base;
        image_size = length;
        uint32_t rva;
        if (!pe_directory(image, image_size, 2, rva, size))
            return fail(why, "no PE32 resource data directory");
        if (!rva || size < 16 || !image_range(rva, size))
            return fail(why, "resource directory is missing or outside the image");
        root = image + rva;
        return true;
    }
    bool entries(uint32_t offset, uint32_t &first, uint32_t &count) {
        if (!relative(offset, 16))
            return fail(why, "truncated resource directory header");
        uint32_t p = root + offset;
        count = (uint32_t)rd16(p + 12) + rd16(p + 14);
        if (!relative(offset + 16, count * 8))
            return fail(why, "truncated resource directory entries");
        first = p + 16;
        return true;
    }
    bool name(uint32_t encoded, ResourceName &out) {
        out = {};
        if (!(encoded & 0x80000000u)) {
            if (encoded > 0xffff)
                return fail(why, "invalid integer resource identifier");
            out.id = encoded;
            return true;
        }
        uint32_t off = encoded & 0x7fffffffu;
        if (!relative(off, 2))
            return fail(why, "truncated resource name length");
        uint32_t units = rd16(root + off);
        if (!relative(off + 2, units * 2))
            return fail(why, "truncated resource name");
        out.is_string = true;
        out.name = gm_wstr(root + off + 2, units);
        return true;
    }
    bool child(uint32_t dir, const ResourceName &wanted, uint32_t &offset) {
        uint32_t first, count;
        if (!entries(dir, first, count))
            return false;
        for (uint32_t i = 0; i < count; ++i) {
            uint32_t p = first + i * 8;
            ResourceName candidate;
            if (!name(rd32(p), candidate))
                return false;
            if (candidate.is_string != wanted.is_string)
                continue;
            bool match = candidate.is_string
                             ? os_strcasecmp(candidate.name.c_str(), wanted.name.c_str()) == 0
                             : candidate.id == wanted.id;
            if (!match)
                continue;
            uint32_t next = rd32(p + 4);
            if (!(next & 0x80000000u))
                return fail(why, "expected a resource subdirectory");
            offset = next & 0x7fffffffu;
            if (!relative(offset, 16))
                return fail(why, "resource subdirectory outside its bounds");
            return true;
        }
        return fail(why, "resource identifier not found");
    }
    uint32_t data(uint32_t entry, uint32_t *bytes) {
        if (entry < root || !relative(entry - root, 16)) {
            fail(why, "invalid resource data entry");
            return 0;
        }
        uint32_t rva = rd32(entry), n = rd32(entry + 4);
        if (!image_range(rva, n)) {
            fail(why, "resource payload outside the image");
            return 0;
        }
        if (bytes)
            *bytes = n;
        return image + rva;
    }
};

} // namespace

uint32_t pe_resource_find(uint32_t base, uint32_t size, const ResourceName &type,
                          const ResourceName &name, std::string *why) {
    Directory dir(why);
    if (!dir.open(base, size))
        return 0;
    uint32_t type_dir, name_dir, first, count;
    if (!dir.child(0, type, type_dir) || !dir.child(type_dir, name, name_dir) ||
        !dir.entries(name_dir, first, count))
        return 0;
    if (!count) {
        fail(why, "resource has no language entry");
        return 0;
    }
    uint32_t data_offset = rd32(first + 4);
    if ((data_offset & 0x80000000u) || !dir.relative(data_offset, 16)) {
        fail(why, "invalid resource language data entry");
        return 0;
    }
    uint32_t entry = dir.root + data_offset;
    return dir.data(entry, nullptr) ? entry : 0;
}
uint32_t pe_resource_data(uint32_t base, uint32_t image_size, uint32_t entry, uint32_t *size,
                          std::string *why) {
    if (size)
        *size = 0;
    Directory dir(why);
    return dir.open(base, image_size) ? dir.data(entry, size) : 0;
}
bool pe_resource_names(uint32_t base, uint32_t size, const ResourceName &type,
                       std::vector<ResourceName> *names, std::string *why) {
    if (!names)
        return fail(why, "missing resource-name output");
    names->clear();
    Directory dir(why);
    uint32_t type_dir, first, count;
    if (!dir.open(base, size) || !dir.child(0, type, type_dir) ||
        !dir.entries(type_dir, first, count))
        return false;
    for (uint32_t i = 0; i < count; ++i) {
        ResourceName name;
        if (!dir.name(rd32(first + i * 8), name)) {
            names->clear();
            return false;
        }
        names->push_back(name);
    }
    return true;
}

bool pe_forwarder(const std::string &forwarder, std::string &module, std::string &name,
                  uint32_t &ordinal, bool &by_ordinal) {
    size_t dot = forwarder.find('.');
    if (dot == std::string::npos || dot == 0 || dot + 1 >= forwarder.size())
        return false;
    module = forwarder.substr(0, dot);
    name = forwarder.substr(dot + 1);
    by_ordinal = name[0] == '#';
    if (by_ordinal) {
        char *end = nullptr;
        long value = strtol(name.c_str() + 1, &end, 10);
        if (!end || *end != '\0')
            return false;
        ordinal = (uint32_t)value;
    }
    return true;
}
