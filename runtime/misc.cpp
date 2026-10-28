// misc.cpp - the smaller import sets: ADVAPI32 (registry), SHELL32, ole32,
// IMM32, WSOCK32 and the non-audio half of WINMM (the millisecond clock,
// multimedia timers and mmio file access). MIDI output is forwarded to the host
// synthesizer; real audio belongs to the audio task.
#include "imports.h"
#include "game_config.h"
#include "layout.h"
#include "memory.h"
#include "win32.h"
#include "loader.h"
#include "resources.h"
#include <algorithm>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../platform/os.h"
#include <atomic>
#include <map>
#include <mutex>
#include <random>
#include <string>
#include <set>
#include <vector>

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------
namespace {
uint64_t g_epoch_us = 0;
uint32_t (*g_time_source)() = nullptr;
// Not owned and not copied: every caller passes a string literal, and this is
// read from a run record writer that may run at exit, by which time anything
// with a lifetime would be a question.
const char *g_clock_desc = nullptr;

uint64_t now_us() {
    return os_wall_time_us();
}
} // namespace

uint32_t host_millis() {
    if (g_time_source)
        return g_time_source();
    if (!g_epoch_us)
        g_epoch_us = now_us();
    return (uint32_t)((now_us() - g_epoch_us) / 1000ull);
}
void host_set_time_source(uint32_t (*fn)()) {
    g_time_source = fn;
}

// ---------------------------------------------------------------------------
// The pinned clock. One counter, here, so that the parity fixture and the boot
// hosts pin the same thing rather than each keeping their own.
//
// Atomic because the guest reads it from every thread it runs, while the host
// advances it from the one that presents. Relaxed is enough: nothing is
// published through it, and a reader a step behind sees a time that was true a
// moment ago, which is the most any clock offers.
// ---------------------------------------------------------------------------
namespace {
std::atomic<bool> g_pin_on{false};
std::atomic<uint32_t> g_pin_ms{0};
uint32_t g_pin_step = 0;
// Static, because host_set_clock_description does not copy: it is read at exit
// by the run record writer, so anything with a shorter life would be a
// question. Sixty-four bytes holds "pinned start=4294967295 step=4294967295".
char g_pin_desc[64];

uint32_t pinned_millis() {
    return g_pin_ms.load(std::memory_order_relaxed);
}
} // namespace

void host_set_time_source_pinned(uint32_t start_ms, uint32_t step_ms) {
    g_pin_ms.store(start_ms, std::memory_order_relaxed);
    g_pin_step = step_ms;
    g_pin_on.store(true, std::memory_order_release);
    snprintf(g_pin_desc, sizeof g_pin_desc, "pinned start=%u step=%u", start_ms, step_ms);
    host_set_clock_description(g_pin_desc);
    host_set_time_source(pinned_millis);
}

void host_pinned_clock_advance() {
    if (!g_pin_on.load(std::memory_order_acquire))
        return;
    g_pin_ms.fetch_add(g_pin_step, std::memory_order_relaxed);
}

void host_clear_time_source() {
    g_pin_on.store(false, std::memory_order_release);
    g_pin_step = 0;
    g_time_source = nullptr;
    g_clock_desc = nullptr; // back to "monotonic"
}

bool host_time_source_is_pinned() {
    return g_pin_on.load(std::memory_order_acquire);
}
uint32_t host_pinned_clock_value() {
    return pinned_millis();
}
uint32_t host_pinned_clock_step() {
    return g_pin_on.load(std::memory_order_acquire) ? g_pin_step : 0u;
}
void host_set_clock_description(const char *text) {
    g_clock_desc = text;
}
const char *host_clock_description() {
    return g_clock_desc ? g_clock_desc : "monotonic";
}

// ---------------------------------------------------------------------------
// The cadence trace.
//
// What it is for: the display work needs to know how often the guest is asking
// the time and how often anything ticks, because a frame rate that looks wrong
// is often a loop that is being paced by something other than frames. Recording
// intervals rather than absolute times is deliberate - two runs start at
// different moments and a diff of absolute stamps is all noise - and measuring
// them on the GUEST's clock rather than the wall is deliberate too, so that a
// pinned run's cadence is the pin's steps and two pinned runs can be compared
// at all.
//
// A note on what is NOT here: this game installs no window timer. There is no
// SetTimer shim in the runtime and no WM_TIMER anywhere in it, so the WM_TIMER
// kind records what reaches host_post_message and nothing else, and a real
// run's trace has no WM_TIMER lines in it. That is an absence of timers, not an
// absence of instrumentation, and the difference matters to whoever reads the
// trace: a missing kind here means the game never did it.
// ---------------------------------------------------------------------------
namespace {
std::mutex g_cadence_mutex;
FILE *g_cadence = nullptr;
// Last event time per kind, on the guest's clock. Small and fixed: three kinds.
struct CadenceKind {
    const char *name;
    uint32_t last;
    bool seen;
};
CadenceKind g_cadence_kinds[] = {
    {"timeGetTime", 0, false}, {"GetTickCount", 0, false}, {"QueryPerformanceCounter", 0, false},
    {"WM_TIMER", 0, false},    {"mm_callback", 0, false},
};
} // namespace

void host_set_cadence_trace(const char *path) {
    std::lock_guard<std::mutex> lock(g_cadence_mutex);
    if (g_cadence) {
        fclose(g_cadence);
        g_cadence = nullptr;
    }
    for (CadenceKind &k : g_cadence_kinds)
        k.seen = false;
    if (!path || !*path)
        return;
    g_cadence = fopen(path, "w");
    if (!g_cadence)
        LOGW("cadence trace: cannot write %s; the run continues untraced", path);
}

void host_note_cadence(const char *kind) {
    if (!g_cadence || !kind)
        return; // the common case, unlocked
    // NOT host_millis(). A boot host's time source is boot_time_source, which
    // counts a poll on every read and takes a stall-breaking step after 256 of
    // them - so reading the clock to timestamp a trace would make a traced run
    // pace differently from an untraced one, and the trace would be measuring
    // its own effect on the thing it measures.
    //
    // Pinned, the counter is readable directly with no side effect. Unpinned,
    // boot_clock_poll() returns immediately and there is no poll to miscount,
    // so host_millis() is safe and is the only way to get a real time.
    uint32_t now = host_time_source_is_pinned() ? host_pinned_clock_value() : host_millis();
    std::lock_guard<std::mutex> lock(g_cadence_mutex);
    if (!g_cadence)
        return; // closed while we waited
    for (CadenceKind &k : g_cadence_kinds) {
        if (strcmp(k.name, kind) != 0)
            continue;
        // The first event of a kind sets the baseline and prints nothing:
        // there is no interval before the first one, and printing a zero
        // would put a gap in the histogram that no run ever had.
        if (k.seen)
            fprintf(g_cadence, "%s,%u\n", k.name, now - k.last);
        k.last = now;
        k.seen = true;
        fflush(g_cadence); // a killed run keeps its trace
        return;
    }
}

// ---------------------------------------------------------------------------
// A very small JSON reader/writer for the registry file. Supports objects,
// strings and integers, which is all the registry backing store needs.
// ---------------------------------------------------------------------------
namespace {

struct JValue {
    enum Kind { OBJ, STR, NUM } kind = OBJ;
    std::map<std::string, JValue> obj;
    std::string str;
    long long num = 0;
};

struct JParser {
    const std::string &s;
    size_t i = 0;
    bool ok = true;
    explicit JParser(const std::string &src) : s(src) {}

    void ws() {
        while (i < s.size() && isspace((unsigned char)s[i]))
            ++i;
    }

    std::string parse_string() {
        std::string out;
        if (i >= s.size() || s[i] != '"') {
            ok = false;
            return out;
        }
        ++i;
        while (i < s.size() && s[i] != '"') {
            char ch = s[i++];
            if (ch == '\\' && i < s.size()) {
                char e = s[i++];
                switch (e) {
                case 'n':
                    out.push_back('\n');
                    break;
                case 't':
                    out.push_back('\t');
                    break;
                case 'r':
                    out.push_back('\r');
                    break;
                case 'u': {
                    if (i + 4 <= s.size()) {
                        int v = (int)strtol(s.substr(i, 4).c_str(), nullptr, 16);
                        i += 4;
                        out.push_back((char)(v & 0xff));
                    }
                    break;
                }
                default:
                    out.push_back(e);
                    break;
                }
            } else {
                out.push_back(ch);
            }
        }
        if (i < s.size())
            ++i;
        return out;
    }

    JValue parse() {
        ws();
        JValue v;
        if (i >= s.size()) {
            ok = false;
            return v;
        }
        if (s[i] == '{') {
            ++i;
            v.kind = JValue::OBJ;
            ws();
            if (i < s.size() && s[i] == '}') {
                ++i;
                return v;
            }
            while (ok && i < s.size()) {
                ws();
                std::string key = parse_string();
                ws();
                if (i < s.size() && s[i] == ':')
                    ++i;
                else {
                    ok = false;
                    break;
                }
                v.obj[key] = parse();
                ws();
                if (i < s.size() && s[i] == ',') {
                    ++i;
                    continue;
                }
                if (i < s.size() && s[i] == '}') {
                    ++i;
                    break;
                }
                ok = false;
            }
        } else if (s[i] == '"') {
            v.kind = JValue::STR;
            v.str = parse_string();
        } else {
            v.kind = JValue::NUM;
            size_t start = i;
            while (i < s.size() && (isdigit((unsigned char)s[i]) || s[i] == '-' || s[i] == '+'))
                ++i;
            if (start == i) {
                ok = false;
                return v;
            }
            v.num = strtoll(s.substr(start, i - start).c_str(), nullptr, 10);
        }
        return v;
    }
};

std::string json_escape(const std::string &s) {
    std::string out;
    for (unsigned char ch : s) {
        switch (ch) {
        case '"':
            out += "\\\"";
            break;
        case '\\':
            out += "\\\\";
            break;
        case '\n':
            out += "\\n";
            break;
        case '\r':
            out += "\\r";
            break;
        case '\t':
            out += "\\t";
            break;
        default:
            if (ch < 0x20 || ch >= 0x7f) {
                char buf[8];
                snprintf(buf, sizeof buf, "\\u%04x", ch);
                out += buf;
            } else {
                out.push_back((char)ch);
            }
        }
    }
    return out;
}

// -------------------------------------------------------------------------
// Registry store
// -------------------------------------------------------------------------
struct RegValue {
    uint32_t type = 1;        // REG_SZ
    std::string str;          // REG_SZ / REG_EXPAND_SZ
    uint32_t dword = 0;       // REG_DWORD
    std::vector<uint8_t> bin; // REG_BINARY and everything else
    // Creation order within its key. Windows enumerates a key's values in the
    // order they were created, and guests depend on it (Black & White's
    // settings reader gives up on the first value that does not fit its 4-byte
    // buffer, so the DWORD settings have to be enumerated before any string).
    uint64_t seq = 0;
};
uint64_t g_reg_seq = 0;

// Registry key paths and value names are case-insensitive on Windows.
struct CiLess {
    bool operator()(const std::string &a, const std::string &b) const {
        return os_strcasecmp(a.c_str(), b.c_str()) < 0;
    }
};
typedef std::map<std::string, RegValue, CiLess> RegValues;

// A key's values in creation order, as RegEnumValue reports them.
std::vector<const std::pair<const std::string, RegValue> *> reg_in_order(const RegValues &v) {
    std::vector<const std::pair<const std::string, RegValue> *> out;
    for (const auto &kv : v)
        out.push_back(&kv);
    std::stable_sort(out.begin(), out.end(),
                     [](const auto *a, const auto *b) { return a->second.seq < b->second.seq; });
    return out;
}

std::map<std::string, RegValues, CiLess> &regstore() {
    static std::map<std::string, RegValues, CiLess> m;
    return m;
}
bool reg_ensure_ancestors(const std::string &path);
std::map<uint32_t, std::string> &regkeys() {
    static std::map<uint32_t, std::string> m;
    return m;
}
// Handles opened without KEY_SET_VALUE. Windows refuses value writes through
// them, and NFS Carbon's FirstTime write through a KEY_READ handle relies on it.
std::set<uint32_t> &regkeys_readonly() {
    static std::set<uint32_t> s;
    return s;
}
uint32_t g_next_hkey = 0x00030004;
bool g_registry_dirty = false;

const char *hive_name(uint32_t h) {
    switch (h) {
    case 0x80000000u:
        return "HKEY_CLASSES_ROOT";
    case 0x80000001u:
        return "HKEY_CURRENT_USER";
    case 0x80000002u:
        return "HKEY_LOCAL_MACHINE";
    case 0x80000003u:
        return "HKEY_USERS";
    case 0x80000005u:
        return "HKEY_CURRENT_CONFIG";
    default:
        return nullptr;
    }
}

std::string key_path(uint32_t hkey, const std::string &sub) {
    std::string base;
    if (const char *h = hive_name(hkey))
        base = h;
    else {
        auto it = regkeys().find(hkey);
        if (it == regkeys().end())
            return std::string();
        base = it->second;
    }
    if (sub.empty())
        return base;
    return base + "\\" + sub;
}

std::vector<uint8_t> hex_to_bytes(const std::string &s) {
    std::vector<uint8_t> out;
    for (size_t i = 0; i + 1 < s.size(); i += 2)
        out.push_back((uint8_t)strtol(s.substr(i, 2).c_str(), nullptr, 16));
    return out;
}

std::string bytes_to_hex(const std::vector<uint8_t> &b) {
    std::string out;
    char buf[3];
    for (uint8_t v : b) {
        snprintf(buf, sizeof buf, "%02x", v);
        out += buf;
    }
    return out;
}

} // namespace

std::string registry_path() {
    if (const char *e = recomp_env("REGISTRY"))
        return e;
    return host_state_file("registry.json");
}

static void registry_load_file() {
    std::string path = registry_path();
    FILE *f = fopen(path.c_str(), "rb");
    if (!f) {
        LOGV("registry: %s does not exist yet, using installation defaults", path.c_str());
        return;
    }
    std::string text;
    char buf[4096];
    size_t n;
    while ((n = fread(buf, 1, sizeof buf, f)) > 0)
        text.append(buf, n);
    fclose(f);

    JParser p(text);
    JValue root = p.parse();
    if (!p.ok || root.kind != JValue::OBJ) {
        LOGW("registry: %s is not valid JSON, ignoring it", path.c_str());
        return;
    }
    for (const auto &key : root.obj) {
        if (key.second.kind != JValue::OBJ)
            continue;
        auto &values = regstore()[key.first];
        for (const auto &val : key.second.obj) {
            if (val.second.kind != JValue::OBJ)
                continue;
            RegValue rv;
            auto t = val.second.obj.find("type");
            if (t != val.second.obj.end() && t->second.kind == JValue::NUM)
                rv.type = (uint32_t)t->second.num;
            auto d = val.second.obj.find("data");
            if (d != val.second.obj.end()) {
                if (rv.type == 4 || rv.type == 5) {
                    rv.dword = d->second.kind == JValue::NUM
                                   ? (uint32_t)d->second.num
                                   : (uint32_t)strtoul(d->second.str.c_str(), nullptr, 0);
                } else if (rv.type == 1 || rv.type == 2) {
                    rv.str = d->second.str;
                } else {
                    rv.bin = hex_to_bytes(d->second.str);
                }
            }
            auto q = val.second.obj.find("seq");
            if (q != val.second.obj.end() && q->second.kind == JValue::NUM) {
                rv.seq = (uint64_t)q->second.num;
                if (rv.seq > g_reg_seq)
                    g_reg_seq = rv.seq;
            }
            values[val.first] = rv;
        }
    }
    // A file written before values carried a sequence has lost its creation
    // order. Give those values one now: DWORDs first, then the rest by name,
    // after everything that has a real sequence. A heuristic for old files only;
    // every value written from now on records its true position.
    for (auto &key : regstore()) {
        std::vector<RegValues::iterator> legacy;
        for (auto it = key.second.begin(); it != key.second.end(); ++it)
            if (!it->second.seq)
                legacy.push_back(it);
        std::stable_sort(legacy.begin(), legacy.end(),
                         [](const RegValues::iterator &a, const RegValues::iterator &b) {
                             const bool ad = a->second.type == 4 || a->second.type == 5;
                             const bool bd = b->second.type == 4 || b->second.type == 5;
                             return ad != bd ? ad : false;
                         });
        for (auto &it : legacy)
            it->second.seq = ++g_reg_seq;
    }
    LOGV("registry: loaded %zu keys from %s", regstore().size(), path.c_str());
}

void registry_load() {
    regstore().clear();
    regkeys().clear();
    regkeys_readonly().clear();
    g_reg_seq = 0;
    g_next_hkey = 0x00030004;
    g_registry_dirty = false;
    registry_load_file();
    const struct {
        const char *key, *name, *text;
        uint32_t dword;
    } defaults[] = RECOMP_REGISTRY_DEFAULTS;
    for (const auto &entry : defaults) {
        if (!entry.key)
            break;
        reg_ensure_ancestors(entry.key);
        auto &values = regstore()[entry.key];
        if (values.count(entry.name))
            continue;
        RegValue value;
        value.type = entry.text ? 1 : 4;
        value.str = entry.text ? entry.text : "";
        value.dword = entry.dword;
        value.seq = ++g_reg_seq;
        values.emplace(entry.name, std::move(value));
    }
}

extern "C" int recomp_registry_dword(const char *key, const char *name, uint32_t *value) {
    auto k = regstore().find(key);
    if (k == regstore().end())
        return 0;
    auto v = k->second.find(name);
    if (v == k->second.end() || v->second.type != 4)
        return 0;
    *value = v->second.dword;
    return 1;
}

extern "C" void recomp_registry_set_dword(const char *key, const char *name, uint32_t value) {
    reg_ensure_ancestors(key);
    auto &values = regstore()[key];
    auto &v = values[name];
    v.type = 4;
    v.dword = value;
    if (!v.seq)
        v.seq = ++g_reg_seq;
    auto ordered = reg_in_order(values);
    std::stable_sort(ordered.begin(), ordered.end(), [](const auto *a, const auto *b) {
        return a->second.type == 4 && b->second.type != 4;
    });
    for (const auto *entry : ordered)
        values[entry->first].seq = ++g_reg_seq;
    g_registry_dirty = true;
    registry_flush();
}

// Persist the registry when changed, creating the destination directories as needed.
// An open failure leaves the dirty flag set so a later flush can retry.
void registry_flush() {
    if (!g_registry_dirty)
        return;
    std::string path = registry_path();
    // Create the parent directory chain (build/recomp) if it is missing.
    size_t pos = 0;
    while ((pos = path.find('/', pos + 1)) != std::string::npos)
        os_mkdir(path.substr(0, pos).c_str());
    FILE *f = fopen(path.c_str(), "wb");
    if (!f) {
        LOGW("registry: cannot write %s", path.c_str());
        return;
    }
    fprintf(f, "{\n");
    bool first_key = true;
    for (const auto &key : regstore()) {
        if (!first_key)
            fprintf(f, ",\n");
        first_key = false;
        fprintf(f, "  \"%s\": {\n", json_escape(key.first).c_str());
        bool first_val = true;
        for (const auto *vp : reg_in_order(key.second)) {
            const auto &val = *vp;
            if (!first_val)
                fprintf(f, ",\n");
            first_val = false;
            const RegValue &rv = val.second;
            if (rv.type == 4 || rv.type == 5)
                fprintf(f, "    \"%s\": {\"type\": %u, \"data\": %u, \"seq\": %llu}",
                        json_escape(val.first).c_str(), rv.type, rv.dword,
                        (unsigned long long)rv.seq);
            else if (rv.type == 1 || rv.type == 2)
                fprintf(f, "    \"%s\": {\"type\": %u, \"data\": \"%s\", \"seq\": %llu}",
                        json_escape(val.first).c_str(), rv.type, json_escape(rv.str).c_str(),
                        (unsigned long long)rv.seq);
            else
                fprintf(f, "    \"%s\": {\"type\": %u, \"data\": \"%s\", \"seq\": %llu}",
                        json_escape(val.first).c_str(), rv.type, bytes_to_hex(rv.bin).c_str(),
                        (unsigned long long)rv.seq);
        }
        fprintf(f, "\n  }");
    }
    fprintf(f, "\n}\n");
    fclose(f);
    g_registry_dirty = false;
}

std::string shell_folder_guest_path(uint32_t csidl, bool create) {
    const char *name;
    switch (csidl & 0xff) {
    case 0x05: // CSIDL_PERSONAL
        name = "Documents";
        break;
    case 0x1a: // CSIDL_APPDATA
    case 0x1c: // CSIDL_LOCAL_APPDATA
        name = "AppData";
        break;
    case 0x23: // CSIDL_COMMON_APPDATA
        name = "CommonAppData";
        break;
    case 0x26: // CSIDL_PROGRAM_FILES
        name = "Program Files";
        break;
    default:
        name = "Documents";
        break;
    }
    std::string guest = std::string(RECOMP_GUEST_ROOT) + "\\" + name;
    if (create) {
        std::string host = win32_host_path(guest, true);
        if (!host.empty())
            os_mkdir(host.c_str());
    }
    return guest;
}

namespace {

// -------------------------------------------------------------------------
// ADVAPI32
// -------------------------------------------------------------------------
// Defined below the open/create entry points; forward-declared so an open can
// see whether a key exists only as an ancestor of stored descendants.
std::vector<std::string> reg_children(const std::string &path);

// Windows creates every missing ancestor when a key is created. The store
// otherwise only holds the exact key paths that were written, so opening the
// parent of a stored subkey - which RegOpenKeyA/LHNetGetProfileList does -
// would fail. Create the ancestors so the tree shape matches the original's.
bool reg_ensure_ancestors(const std::string &path) {
    if (path.empty())
        return false;
    size_t first = path.find('\\');
    if (first == std::string::npos)
        return false; // a predefined hive root is implicit, never stored
    bool added = false;
    for (size_t i = path.find('\\', first + 1); i != std::string::npos;
         i = path.find('\\', i + 1)) {
        if (regstore().emplace(path.substr(0, i), RegValues{}).second)
            added = true;
    }
    return added;
}

// KEY_SET_VALUE, or a generic right that maps to it.
bool reg_sam_can_set(uint32_t sam) {
    return sam & (0x2u | 0x02000000u | 0x10000000u | 0x40000000u);
}

void a_RegOpenKeyEx(X86 *c, uint32_t hkey, const std::string &sub, uint32_t sam, uint32_t presult) {
    std::string path = key_path(hkey, sub);
    if (path.empty()) {
        set_eax(c, 6);
        return;
    } // ERROR_INVALID_HANDLE
    // Predefined roots exist independently of serialized subkeys. Opening
    // one with a null/empty subkey returns that same predefined handle.
    if (sub.empty() && hive_name(hkey)) {
        if (presult)
            wr32(presult, hkey);
        set_eax(c, 0);
        return;
    }
    // A key with descendants exists even when no value was ever written at it
    // and it was never the target of a create call.
    if (regstore().find(path) == regstore().end() && reg_children(path).empty()) {
        LOGV("RegOpenKeyExA(%s): not found", path.c_str());
        set_eax(c, 2); // ERROR_FILE_NOT_FOUND
        return;
    }
    uint32_t h = g_next_hkey;
    g_next_hkey += 4;
    regkeys()[h] = path;
    if (!reg_sam_can_set(sam))
        regkeys_readonly().insert(h);
    if (presult)
        wr32(presult, h);
    set_eax(c, 0);
}

void a_RegOpenKeyA(X86 *c) {
    a_RegOpenKeyEx(c, arg(c, 0), gm_str(arg(c, 1), 512), 0x02000000u, arg(c, 2));
}
void a_RegOpenKeyExA(X86 *c) {
    a_RegOpenKeyEx(c, arg(c, 0), gm_str(arg(c, 1), 512), arg(c, 3), arg(c, 4));
}

void reg_create(X86 *c, const std::string &sub) {
    std::string path = key_path(arg(c, 0), sub);
    uint32_t presult = arg(c, 7), pdisp = arg(c, 8);
    if (path.empty()) {
        set_eax(c, 6);
        return;
    }
    bool existed = regstore().find(path) != regstore().end();
    if (reg_ensure_ancestors(path) || !existed) {
        regstore()[path];
        g_registry_dirty = true;
    }
    uint32_t h = g_next_hkey;
    g_next_hkey += 4;
    regkeys()[h] = path;
    if (!reg_sam_can_set(arg(c, 5)))
        regkeys_readonly().insert(h);
    if (presult)
        wr32(presult, h);
    if (pdisp)
        wr32(pdisp, existed ? 2 : 1); // OPENED_EXISTING_KEY / CREATED_NEW_KEY
    set_eax(c, 0);
}

// The pre-Win32s spelling: no options, no access mask, no disposition. It
// creates or opens exactly as RegCreateKeyExA does, because the store has no
// notion of either.
void a_RegCreateKeyA(X86 *c) {
    std::string path = key_path(arg(c, 0), gm_str(arg(c, 1), 512));
    uint32_t presult = arg(c, 2);
    if (path.empty()) {
        set_eax(c, 6);
        return;
    }
    if (reg_ensure_ancestors(path) || regstore().find(path) == regstore().end()) {
        regstore()[path];
        g_registry_dirty = true;
    }
    uint32_t h = g_next_hkey;
    g_next_hkey += 4;
    regkeys()[h] = path;
    if (presult)
        wr32(presult, h);
    set_eax(c, 0);
}

void a_RegCloseKey(X86 *c) {
    regkeys().erase(arg(c, 0));
    regkeys_readonly().erase(arg(c, 0));
    registry_flush();
    set_eax(c, 0);
}

// The backing store is UTF-8. W sizes are UTF-16 units for names and
// UTF-16 bytes (including NUL) for REG_SZ/REG_EXPAND_SZ data.
uint32_t reg_value_size(const RegValue &rv, bool wide) {
    if (rv.type == 1 || rv.type == 2)
        return wide ? (utf16_units(rv.str) + 1) * 2 : uint32_t(rv.str.size() + 1);
    return rv.type == 4 || rv.type == 5 ? 4 : uint32_t(rv.bin.size());
}
uint32_t reg_read_value(const RegValue &rv, bool wide, uint32_t type, uint32_t data,
                        uint32_t size) {
    if ((type && !gm_valid(type, 4)) || (size && !gm_valid(size, 4)) || (data && !size))
        return 87;
    uint32_t need = reg_value_size(rv, wide), have = size ? rd32(size) : 0;
    if (type)
        wr32(type, rv.type);
    if (size)
        wr32(size, need);
    if (!data)
        return 0;
    if (have < need)
        return 234;
    if (!gm_valid(data, need))
        return 87;
    if (rv.type == 1 || rv.type == 2) {
        if (wide)
            gm_put_wstr(data, rv.str, need / 2);
        else
            gm_put_str(data, rv.str.c_str(), need);
    } else if (rv.type == 4 || rv.type == 5)
        wr32(data, rv.dword);
    else if (need)
        memcpy(g_mem + data, rv.bin.data(), need);
    return 0;
}

void reg_query(X86 *c, const std::string &name, bool wide) {
    uint32_t hkey = arg(c, 0);
    uint32_t ptype = arg(c, 3), pdata = arg(c, 4), pcb = arg(c, 5);

    auto ki = regkeys().find(hkey);
    std::string path =
        ki != regkeys().end() ? ki->second : std::string(hive_name(hkey) ? hive_name(hkey) : "");
    if (path.empty()) {
        LOGV("registry: query 0x%08x \"%s\": no such key handle", hkey, name.c_str());
        set_eax(c, 6);
        return;
    }
    auto si = regstore().find(path);
    if (si == regstore().end()) {
        LOGV("registry: query %s \"%s\": key absent", path.c_str(), name.c_str());
        set_eax(c, 2);
        return;
    }
    auto vi = si->second.find(name);
    if (vi == si->second.end()) {
        LOGV("registry: query %s \"%s\": value absent", path.c_str(), name.c_str());
        set_eax(c, 2);
        return;
    }
    LOGV("registry: query %s \"%s\": found", path.c_str(), name.c_str());

    set_eax(c, reg_read_value(vi->second, wide, ptype, pdata, pcb));
}

void reg_set(X86 *c, const std::string &name, bool wide) {
    uint32_t hkey = arg(c, 0);
    uint32_t type = arg(c, 3), pdata = arg(c, 4), cb = arg(c, 5);
    if (regkeys_readonly().count(hkey)) {
        set_eax(c, 5); // ERROR_ACCESS_DENIED
        return;
    }

    auto ki = regkeys().find(hkey);
    std::string path =
        ki != regkeys().end() ? ki->second : std::string(hive_name(hkey) ? hive_name(hkey) : "");
    if (path.empty()) {
        set_eax(c, 6);
        return;
    }

    RegValue rv;
    rv.type = type;
    if (type == 4 || type == 5) {
        rv.dword = cb >= 4 && pdata ? rd32(pdata) : 0;
    } else if (type == 1 || type == 2) {
        if ((wide && (cb & 1)) || (pdata && !gm_valid(pdata, cb))) {
            set_eax(c, 87);
            return;
        }
        rv.str = pdata ? (wide ? gm_wstr(pdata, cb / 2) : gm_str(pdata, cb)) : std::string();
    } else if (pdata) {
        rv.bin.assign(g_mem + pdata, g_mem + pdata + cb);
    }
    if (reg_ensure_ancestors(path))
        g_registry_dirty = true;
    {
        auto &vals = regstore()[path];
        auto old = vals.find(name);
        rv.seq = old != vals.end() ? old->second.seq : ++g_reg_seq; // overwriting keeps its place
        vals[name] = rv;
    }
    if (log_level() >= 2) {
        char numbuf[32];
        const char *valdesc = "<binary>";
        if (type == 4 || type == 5) {
            snprintf(numbuf, sizeof numbuf, "%u", rv.dword);
            valdesc = numbuf;
        } else if (type == 1 || type == 2) {
            valdesc = rv.str.c_str();
        }
        LOGV("registry: set %s \"%s\" type=%u value=%s", path.c_str(), name.c_str(), type, valdesc);
    }
    g_registry_dirty = true;
    registry_flush();
    set_eax(c, 0);
}

void a_RegOpenKeyExW(X86 *c) {
    a_RegOpenKeyEx(c, arg(c, 0), gm_wstr(arg(c, 1)), arg(c, 3), arg(c, 4));
}
void a_RegCreateKeyExA(X86 *c) {
    reg_create(c, gm_str(arg(c, 1), 512));
}
void a_RegCreateKeyExW(X86 *c) {
    reg_create(c, gm_wstr(arg(c, 1)));
}
void a_RegQueryValueExA(X86 *c) {
    reg_query(c, gm_str(arg(c, 1), 256), false);
}
// RegQueryValueA is the pre-RegQueryValueEx form of the same read: it returns
// the unnamed value of hKey, or of lpSubKey when one is given, as ANSI text.
void a_RegQueryValueA(X86 *c) {
    std::string path = key_path(arg(c, 0), gm_str(arg(c, 1), 512));
    if (path.empty()) {
        set_eax(c, 6);
        return;
    }
    auto it = regstore().find(path);
    if (it == regstore().end()) {
        set_eax(c, 2);
        return;
    }
    auto vi = it->second.find("");
    if (vi == it->second.end()) {
        set_eax(c, 2);
        return;
    }
    set_eax(c, reg_read_value(vi->second, false, 0, arg(c, 2), arg(c, 3)));
}
void a_RegQueryValueExW(X86 *c) {
    reg_query(c, gm_wstr(arg(c, 1)), true);
}
void a_RegSetValueExA(X86 *c) {
    reg_set(c, gm_str(arg(c, 1), 256), false);
}
void a_RegSetValueExW(X86 *c) {
    reg_set(c, gm_wstr(arg(c, 1)), true);
}

// Descendants may have been created in one call to RegCreateKeyEx. Enumerate
// each immediate child once, even when the store only holds its descendant.
std::vector<std::string> reg_children(const std::string &path) {
    std::set<std::string, CiLess> names;
    std::string prefix = path + "\\";
    for (const auto &kv : regstore()) {
        if (kv.first.size() > prefix.size() &&
            !os_strcasecmp(kv.first.substr(0, prefix.size()).c_str(), prefix.c_str())) {
            std::string tail = kv.first.substr(prefix.size());
            names.insert(tail.substr(0, tail.find('\\')));
        }
    }
    return {names.begin(), names.end()};
}
uint32_t reg_write_name(const std::string &name, uint32_t out, uint32_t len) {
    if (!len || !gm_valid(len, 4))
        return 87;
    uint32_t have = rd32(len), need = utf16_units(name);
    wr32(len, need);
    if (!out || have <= need)
        return 234;
    if (!gm_valid(out, (need + 1) * 2))
        return 87;
    gm_put_wstr(out, name, have);
    return 0;
}
void a_RegEnumKeyExW(X86 *c) {
    std::string path = key_path(arg(c, 0), "");
    if (path.empty()) {
        set_eax(c, 6);
        return;
    }
    auto names = reg_children(path);
    if (arg(c, 1) >= names.size()) {
        set_eax(c, 259);
        return;
    }
    uint32_t hr = reg_write_name(names[arg(c, 1)], arg(c, 2), arg(c, 3));
    if (!hr && arg(c, 6))
        hr = reg_write_name("", arg(c, 5), arg(c, 6));
    if (!hr && arg(c, 7) && gm_valid(arg(c, 7), 8))
        wr64(arg(c, 7), 0);
    set_eax(c, hr);
}
void a_RegEnumValueW(X86 *c) {
    std::string path = key_path(arg(c, 0), "");
    if (path.empty()) {
        set_eax(c, 6);
        return;
    }
    auto it = regstore().find(path);
    if (it == regstore().end() || arg(c, 1) >= it->second.size()) {
        set_eax(c, 259);
        return;
    }
    const auto ordered = reg_in_order(it->second);
    const auto *value = ordered[arg(c, 1)];
    const uint32_t name_cap = arg(c, 3) && gm_valid(arg(c, 3), 4) ? rd32(arg(c, 3)) : 0;
    uint32_t hr = reg_write_name(value->first, arg(c, 2), arg(c, 3));
    if (!hr) {
        hr = reg_read_value(value->second, true, arg(c, 5), arg(c, 6), arg(c, 7));
        // A data buffer that is too small fails the call without touching the
        // name length: callers retry with the same variable (LHLogR's
        // RegistryRetrieveVal sets it once, before its first call).
        if (hr == 234 && arg(c, 3))
            wr32(arg(c, 3), name_cap);
    }
    set_eax(c, hr);
}
// The ANSI spelling of the same enumeration: value names are the UTF-8 the
// store keeps, written byte-wide, and the string data is read back byte-wide.
// LHLogR enumerates its registry values in ANSI; leaving this unregistered
// aborted the DLL's logging init with an unknown stdcall arity.
uint32_t reg_write_name_a(const std::string &name, uint32_t out, uint32_t len) {
    if (!len || !gm_valid(len, 4))
        return 87;
    uint32_t have = rd32(len), need = uint32_t(name.size());
    wr32(len, need);
    if (!out || have <= need)
        return 234;
    if (!gm_valid(out, need + 1))
        return 87;
    gm_put_str(out, name.c_str(), have);
    return 0;
}
// RegEnumKeyA is the 4-argument ANSI enumerator LHMultiplayerR's profile list
// uses: child names are the UTF-8 the store keeps, written byte-wide.
void a_RegEnumKeyA(X86 *c) {
    std::string path = key_path(arg(c, 0), "");
    if (path.empty()) {
        set_eax(c, 6);
        return;
    }
    auto names = reg_children(path);
    if (arg(c, 1) >= names.size()) {
        set_eax(c, 259);
        return;
    }
    // Unlike RegEnumValue, RegEnumKey's last argument is the buffer size in
    // bytes by value (including the NUL), not a pointer to it.
    const std::string &name = names[arg(c, 1)];
    uint32_t out = arg(c, 2), cch = arg(c, 3);
    if (!out || cch <= name.size()) {
        set_eax(c, 234); // ERROR_MORE_DATA
        return;
    }
    if (!gm_valid(out, uint32_t(name.size()) + 1)) {
        set_eax(c, 87);
        return;
    }
    gm_put_str(out, name.c_str(), cch);
    set_eax(c, 0);
}
void a_RegEnumValueA(X86 *c) {
    std::string path = key_path(arg(c, 0), "");
    if (path.empty()) {
        set_eax(c, 6);
        return;
    }
    auto it = regstore().find(path);
    if (it == regstore().end() || arg(c, 1) >= it->second.size()) {
        set_eax(c, 259);
        return;
    }
    const auto ordered = reg_in_order(it->second);
    const auto *value = ordered[arg(c, 1)];
    const uint32_t name_cap = arg(c, 3) && gm_valid(arg(c, 3), 4) ? rd32(arg(c, 3)) : 0;
    uint32_t hr = reg_write_name_a(value->first, arg(c, 2), arg(c, 3));
    if (!hr) {
        hr = reg_read_value(value->second, false, arg(c, 5), arg(c, 6), arg(c, 7));
        // See RegEnumValueW: a too-small data buffer leaves the name length as
        // the caller set it, so its retry still has the whole name buffer.
        if (hr == 234 && arg(c, 3))
            wr32(arg(c, 3), name_cap);
    }
    LOGV("registry: enum %s #%u \"%s\" -> %u", path.c_str(), arg(c, 1), value->first.c_str(), hr);
    set_eax(c, hr);
}
void reg_query_info_key(X86 *c, bool wide) {
    std::string path = key_path(arg(c, 0), "");
    if (path.empty()) {
        set_eax(c, 6);
        return;
    }
    auto keys = reg_children(path);
    auto it = regstore().find(path);
    uint32_t maxkey = 0, maxname = 0, maxdata = 0, count = 0;
    for (const auto &name : keys)
        maxkey = std::max(maxkey, wide ? utf16_units(name) : uint32_t(name.size()));
    if (it != regstore().end()) {
        count = uint32_t(it->second.size());
        for (const auto &v : it->second) {
            maxname = std::max(maxname, wide ? utf16_units(v.first) : uint32_t(v.first.size()));
            maxdata = std::max(maxdata, reg_value_size(v.second, wide));
        }
    }
    uint32_t hr = 0;
    if (arg(c, 2)) {
        if (arg(c, 1))
            hr = wide ? reg_write_name("", arg(c, 1), arg(c, 2))
                      : reg_write_name_a("", arg(c, 1), arg(c, 2));
        else
            wr32(arg(c, 2), 0);
    }
    uint32_t values[] = {uint32_t(keys.size()), maxkey, 0, count, maxname, maxdata, 0};
    for (int i = 0; i < 7; ++i)
        if (arg(c, i + 4))
            wr32(arg(c, i + 4), values[i]);
    if (arg(c, 11))
        wr64(arg(c, 11), 0);
    set_eax(c, hr);
}
// The ANSI spelling of the same query; LHMultiplayerR's profile list sizes its
// child-name buffer from it before RegEnumKeyA.
void a_RegQueryInfoKeyA(X86 *c) {
    reg_query_info_key(c, false);
}
void a_RegQueryInfoKeyW(X86 *c) {
    reg_query_info_key(c, true);
}
// Shared by both spellings. On NT a key that still has subkeys is refused with
// ERROR_ACCESS_DENIED and is never deleted recursively; LHNetDeleteProfile
// deletes the profile's own subkey, which holds values, not child keys.
void reg_delete_key(X86 *c, const std::string &path) {
    if (path.empty()) {
        set_eax(c, 6);
        return;
    }
    if (!reg_children(path).empty()) {
        set_eax(c, 5);
        return;
    }
    if (!regstore().erase(path)) {
        set_eax(c, 2);
        return;
    }
    g_registry_dirty = true;
    set_eax(c, 0);
}
void a_RegDeleteKeyA(X86 *c) {
    reg_delete_key(c, key_path(arg(c, 0), gm_str(arg(c, 1), 512)));
}
void a_RegDeleteKeyW(X86 *c) {
    reg_delete_key(c, key_path(arg(c, 0), gm_wstr(arg(c, 1))));
}
void a_RegDeleteValueW(X86 *c) {
    std::string path = key_path(arg(c, 0), "");
    if (path.empty()) {
        set_eax(c, 6);
        return;
    }
    if (regkeys_readonly().count(arg(c, 0))) {
        set_eax(c, 5);
        return;
    }
    auto it = regstore().find(path);
    if (it == regstore().end() || !it->second.erase(gm_wstr(arg(c, 1)))) {
        set_eax(c, 2);
        return;
    }
    g_registry_dirty = true;
    set_eax(c, 0);
}
void a_RegFlushKey(X86 *c) {
    if (key_path(arg(c, 0), "").empty()) {
        set_eax(c, 6);
        return;
    }
    registry_flush();
    set_eax(c, g_registry_dirty ? 5 : 0);
}
void a_RegDenied(X86 *c) {
    set_eax(c, 5);
}

// -------------------------------------------------------------------------
// SHELL32 / ole32
// -------------------------------------------------------------------------
// SHGetSpecialFolderPathA(hwnd, path, csidl, create): the per-user folders a
// game keeps its settings and saves in.  They live under the guest root, so
// they resolve like every other game path (and the mod layer's file seam can
// redirect them); the folder is created when asked.
//
// SHGetFolderPathA(hwnd, csidl, hToken, dwFlags, pszPath) is the same folders
// in the HRESULT form, and CSIDL_FLAG_CREATE (0x8000) asks for one to exist.
void s_SHGetFolderPathA(X86 *c) {
    uint32_t csidl = arg(c, 1), buf = arg(c, 4);
    if (!buf) {
        set_eax(c, 0x80070057u); // E_INVALIDARG
        return;
    }
    std::string guest = shell_folder_guest_path(csidl, (csidl & 0x8000u) != 0);
    gm_put_str(buf, guest.c_str(), 260);
    set_eax(c, 0); // S_OK
}

template <bool Wide> void s_SHGetSpecialFolderPath(X86 *c) {
    uint32_t buf = arg(c, 1);
    if (!buf || !gm_valid(buf, Wide ? 520 : 260)) {
        set_eax(c, 0);
        return;
    }
    uint32_t csidl = arg(c, 2);
    switch (csidl & 0xff) {
    case 0x05:
    case 0x1a:
    case 0x1c:
    case 0x23:
    case 0x26:
        break;
    default:
        LOGW("SHGetSpecialFolderPath: unsupported CSIDL %#x", csidl);
        imports_unsupported(c);
        return;
    }
    std::string guest = shell_folder_guest_path(csidl, arg(c, 3) != 0);
    if constexpr (Wide)
        gm_put_wstr(buf, guest, 260);
    else
        gm_put_str(buf, guest.c_str(), 260);
    set_eax(c, 1);
}

void s_ShellExecuteA(X86 *c) {
    LOGW("ShellExecuteA(\"%s\", \"%s\"): nothing is launched from the runtime",
         gm_str(arg(c, 1), 64).c_str(), gm_str(arg(c, 2), 260).c_str());
    set_eax(c, 2); // ERROR_FILE_NOT_FOUND: the shell did not start anything
}

void o_CoInitialize(X86 *c) {
    set_eax(c, 0);
} // S_OK
void o_CoUninitialize(X86 *c) {
    set_eax(c, 0);
}
void o_CoLockObjectExternal(X86 *c) {
    log_once("ole32:CoLockObjectExternal",
             "ole32!CoLockObjectExternal: external COM locking is not modelled; S_OK");
    set_eax(c, 0);
}
void o_RegisterDragDrop(X86 *c) {
    log_once("ole32:RegisterDragDrop",
             "ole32!RegisterDragDrop: OLE drag-and-drop is not modelled; S_OK");
    set_eax(c, 0);
}
void o_RevokeDragDrop(X86 *c) {
    log_once("ole32:RevokeDragDrop",
             "ole32!RevokeDragDrop: OLE drag-and-drop is not modelled; S_OK");
    set_eax(c, 0);
}

// Generate a version-4 GUID using host entropy, without consuming guest RNG.
// GUID Data1/2/3 are little endian, so the version nibble resides in byte 7.
void o_CoCreateGuid(X86 *c) {
    uint32_t out = arg(c, 0);
    if (!out || !gm_valid(out, 16)) {
        set_eax(c, 0x80070057); // E_INVALIDARG
        return;
    }
    uint8_t bytes[16];
    try {
        std::random_device entropy;
        for (unsigned i = 0; i < 16; i += 4) {
            uint32_t word = entropy();
            for (unsigned j = 0; j < 4; ++j)
                bytes[i + j] = (uint8_t)(word >> (j * 8));
        }
    } catch (...) {
        LOGW("CoCreateGuid: host entropy unavailable");
        set_eax(c, 0x80004005); // E_FAIL
        return;
    }
    bytes[7] = (bytes[7] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    memcpy(gm_ptr(out), bytes, sizeof(bytes));
    set_eax(c, 0); // S_OK
}

void o_CoTaskMemAlloc(X86 *c) {
    set_eax(c, heap_alloc(arg(c, 0)));
}
void o_CoTaskMemFree(X86 *c) {
    if (arg(c, 0))
        heap_free(arg(c, 0));
    set_eax(c, 0);
}
// StringFromCLSID(rclsid, &lplpsz): the canonical braces-and-dashes form, in
// guest memory allocated with CoTaskMemAlloc, which heap_alloc backs.
void o_StringFromCLSID(X86 *c) {
    uint32_t clsid = arg(c, 0), out = arg(c, 1);
    if (!clsid || !gm_valid(clsid, 16) || !out || !gm_valid(out, 4)) {
        set_eax(c, 0x80070057u); // E_INVALIDARG
        return;
    }
    const uint8_t *g = gm_ptr(clsid);
    uint32_t d1;
    uint16_t d2, d3;
    memcpy(&d1, g, 4);
    memcpy(&d2, g + 4, 2);
    memcpy(&d3, g + 6, 2);
    char text[40];
    snprintf(text, sizeof text, "{%08X-%04X-%04X-%02X%02X-%02X%02X%02X%02X%02X%02X}", d1, d2, d3,
             g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]);
    uint32_t units = (uint32_t)strlen(text);
    uint32_t buf = heap_alloc((units + 1) * 2, true);
    if (!buf) {
        set_eax(c, 0x8007000eu); // E_OUTOFMEMORY
        return;
    }
    gm_put_wstr(buf, text, units + 1);
    wr32(out, buf);
    set_eax(c, 0);
}
void o_IsEqualGUID(X86 *c) {
    uint32_t a = arg(c, 0), b = arg(c, 1);
    set_eax(c, a && b && gm_valid(a, 16) && gm_valid(b, 16) && !memcmp(g_mem + a, g_mem + b, 16));
}

// CoFileTimeToDosDateTime(FILETIME *pft, WORD *pFatDate, WORD *pFatTime).
// Documented ole32 API: a FILETIME is unsigned 100 ns ticks since 1601-01-01;
// the DOS date word is ((year-1980)<<9)|(month<<5)|day and the time word is
// (hour<<11)|(minute<<5)|(second/2). FALSE outside the representable range.
// The exe caller passes the file time and the two output words, so this is a
// real conversion, not a stub.
void o_CoFileTimeToDosDateTime(X86 *c) {
    uint32_t ft = arg(c, 0), pdate = arg(c, 1), ptime = arg(c, 2);
    if (!ft || !gm_valid(ft, 8) || !pdate || !ptime || !gm_valid(pdate, 2) || !gm_valid(ptime, 2)) {
        set_eax(c, 0);
        return;
    }
    const uint64_t ticks = (uint64_t)rd32(ft) | ((uint64_t)rd32(ft + 4) << 32);
    const int64_t days = (int64_t)(ticks / 864000000000ull);
    const int64_t secs = (int64_t)((ticks % 864000000000ull) / 10000000ull);
    // Days since 1601-01-01 -> civil date (Howard Hinnant's civil_from_days).
    int64_t z = days - 134774; // 1601 epoch to 1970 epoch
    z += 719468;
    const int64_t era = (z >= 0 ? z : z - 146096) / 146097;
    const int64_t doe = z - era * 146097;
    const int64_t yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    int64_t year = yoe + era * 400;
    const int64_t doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    const int64_t mp = (5 * doy + 2) / 153;
    const int64_t day = doy - (153 * mp + 2) / 5 + 1;
    const int64_t month = mp < 10 ? mp + 3 : mp - 9;
    year += (month <= 2);
    if (year < 1980 || year > 2107) {
        set_eax(c, 0);
        return;
    }
    const int64_t hour = secs / 3600, minute = (secs % 3600) / 60, second = secs % 60;
    wr16(pdate, (uint16_t)(((year - 1980) << 9) | (month << 5) | day));
    wr16(ptime, (uint16_t)((hour << 11) | (minute << 5) | (second / 2)));
    set_eax(c, 1);
}

// PROPVARIANT is sixteen bytes on x86: a two-byte VARTYPE, six reserved, and
// an eight-byte union. Clearing one means releasing whatever the union owns
// and then emptying it, and every PROPVARIANT the shims hand out is VT_EMPTY -
// a media event's value, a property nobody set - so emptying it is the whole
// of the work. A caller that never sees an owning variant cannot leak one.
//
// This is delay-loaded by Delphi's Media Foundation player, and an unresolved
// delay import does not fail quietly: the stub raises 0xC06D007F, which
// surfaces as an "external exception" dialog and takes the process with it.
const uint32_t PROPVARIANT_SIZE = 16;
void o_PropVariantClear(X86 *c) {
    const uint32_t pv = arg(c, 0);
    if (!pv || !gm_valid(pv, PROPVARIANT_SIZE)) {
        set_eax(c, 0x80070057u); // E_INVALIDARG
        return;
    }
    memset(g_mem + pv, 0, PROPVARIANT_SIZE);
    set_eax(c, 0);
}
void o_PropVariantCopy(X86 *c) {
    const uint32_t dst = arg(c, 0), src = arg(c, 1);
    if (!dst || !src || !gm_valid(dst, PROPVARIANT_SIZE) || !gm_valid(src, PROPVARIANT_SIZE)) {
        set_eax(c, 0x80070057u); // E_INVALIDARG
        return;
    }
    memmove(g_mem + dst, g_mem + src, PROPVARIANT_SIZE);
    set_eax(c, 0);
}

// -------------------------------------------------------------------------
// IMM32: no input method is attached.
// -------------------------------------------------------------------------
// NULL hwnd asks for classification only, regardless of attached IME state.
// The message classes follow ImmIsUIMessage's published API and Wine's IMM
// compatibility implementation (dlls/imm32/imm.c).
void i_ImmIsUIMessageA(X86 *c) {
    uint32_t hwnd = arg(c, 0), msg = arg(c, 1);
    bool ime = (msg >= 0x10d && msg <= 0x10f) || msg == 0x281 || msg == 0x282 || msg == 0x284 ||
               msg == 0x285 || msg == 0x287;
    if (ime && hwnd) {
        uint32_t args[] = {hwnd, msg, arg(c, 2), arg(c, 3)};
        guest_call(c, imports_resolve("USER32.dll", "SendMessageA"), args, 4);
    }
    set_eax(c, ime ? 1 : 0);
}
void i_ImmGetContext(X86 *c) {
    set_eax(c, 0);
}
void i_ImmReleaseContext(X86 *c) {
    set_eax(c, 1);
}
void i_ImmGetOpenStatus(X86 *c) {
    set_eax(c, 0);
}
void i_ImmSetOpenStatus(X86 *c) {
    set_eax(c, 1);
}
void i_ImmGetCompositionStringA(X86 *c) {
    set_eax(c, 0);
}
void i_ImmGetCandidateListA(X86 *c) {
    set_eax(c, 0);
}
void i_ImmSetCompositionWindow(X86 *c) {
    set_eax(c, 1);
}
// runblack.exe imports these five and the four above; the host has no input
// method, so there is no context to return or destroy. fn_007F4300 (called by
// pc_main) is `ImmAssociateContext(hwnd, NULL)` with the result discarded.
void i_ImmAssociateContext(X86 *c) {
    // (HWND, HIMC) -> the previous HIMC; there was none, and the caller in
    // fn_007F4300 discards it.
    set_eax(c, 0);
}
void i_ImmCreateContext(X86 *c) {
    set_eax(c, 0);
}
void i_ImmDestroyContext(X86 *c) {
    set_eax(c, 1);
}
void i_ImmGetProperty(X86 *c) {
    set_eax(c, 0);
}
void i_ImmGetDescriptionA(X86 *c) {
    set_eax(c, 0);
}
void i_ImmDisableTextFrameService(X86 *c) {
    set_eax(c, 1);
}

// -------------------------------------------------------------------------
// WSOCK32: name resolution against the host. Real networking (sockets) is a
// later task; host-name lookup is what games probe at startup to discover the
// local machine's own address.
// -------------------------------------------------------------------------
static int g_wsa_last_error = 0;
static constexpr int WSAEFAULT_ = 10014;
static constexpr int WSAHOST_NOT_FOUND_ = 11001;

void w_WSAGetLastError(X86 *c) {
    set_eax(c, (uint32_t)g_wsa_last_error);
}
void w_WSASetLastError(X86 *c) {
    g_wsa_last_error = (int)arg(c, 0);
}

void w_WSAStartup(X86 *c) {
    uint32_t version = arg(c, 0), pdata = arg(c, 1);
    if (pdata) {
        memset(g_mem + pdata, 0, 400);
        wr16(pdata + 0, (uint16_t)version);
        wr16(pdata + 2, 0x0202);
        gm_put_str(pdata + 4, "pop-metal recomp sockets", 257);
        gm_put_str(pdata + 4 + 257, "Running", 129);
        wr16(pdata + 4 + 257 + 129, 32);       // iMaxSockets
        wr16(pdata + 4 + 257 + 129 + 2, 1024); // iMaxUdpDg
    }
    set_eax(c, 0);
}
void w_WSACleanup(X86 *c) {
    set_eax(c, 0);
}
void w_gethostname(X86 *c) {
    uint32_t buf = arg(c, 0), len = arg(c, 1);
    if (!buf || !len || !gm_valid(buf, len)) {
        g_wsa_last_error = WSAEFAULT_;
        set_eax(c, (uint32_t)-1); // SOCKET_ERROR
        return;
    }
    // Fidelity: the real Winsock returns the machine's own name, which a game
    // then resolves with gethostbyname. The earlier stub returned
    // RECOMP_GAME_ID, a name no resolver can answer, so that chain always
    // failed; the host's name is what the original would have returned.
    if (os_hostname((char *)gm_ptr(buf), len) != 0) {
        g_wsa_last_error = WSAHOST_NOT_FOUND_;
        set_eax(c, (uint32_t)-1); // SOCKET_ERROR
        return;
    }
    set_eax(c, 0);
}

// gethostbyname hands back a per-thread block that the next call on the same
// thread overwrites, exactly like Winsock's thread-local result. It lives in
// the guest arena so the pointers are guest-addressable; the blocks are reused
// so a caller that keeps the pointer until its next lookup sees a stable
// address.
struct GethostCache {
    uint32_t hostent = 0;
    uint32_t name = 0;
    uint32_t addrs = 0;
    uint32_t in_addr = 0;
};
static thread_local GethostCache g_gethost;

static bool gethost_cache_live() {
    return g_gethost.hostent && heap_owns(g_gethost.hostent) && heap_owns(g_gethost.name) &&
           heap_owns(g_gethost.addrs) && heap_owns(g_gethost.in_addr);
}

void w_gethostbyname(X86 *c) {
    uint32_t namep = arg(c, 0);
    std::string name = namep ? gm_str(namep, 256) : std::string();
    unsigned char addr[4];
    if (!name.empty() && os_resolve_ipv4(name.c_str(), addr) == 0) {
        if (!gethost_cache_live()) {
            GethostCache fresh;
            fresh.hostent = heap_alloc(16, true);
            fresh.name = heap_alloc(256, true);
            fresh.addrs = heap_alloc(8, true);
            fresh.in_addr = heap_alloc(4, true);
            if (fresh.hostent && fresh.name && fresh.addrs && fresh.in_addr) {
                // struct hostent: h_name, h_aliases, h_addrtype (AF_INET),
                // h_length (4), h_addr_list (NULL-terminated array).
                wr32(fresh.hostent + 0, fresh.name);
                wr32(fresh.hostent + 4, 0);
                wr16(fresh.hostent + 8, 2);
                wr16(fresh.hostent + 10, 4);
                wr32(fresh.hostent + 12, fresh.addrs);
                wr32(fresh.addrs + 0, fresh.in_addr);
                wr32(fresh.addrs + 4, 0);
                g_gethost = fresh;
            }
        }
        if (gethost_cache_live()) {
            gm_put_str(g_gethost.name, name.c_str(), 256);
            memcpy(gm_ptr(g_gethost.in_addr), addr, 4);
            set_eax(c, g_gethost.hostent);
            return;
        }
    }
    g_wsa_last_error = WSAHOST_NOT_FOUND_;
    LOGV("gethostbyname(\"%s\"): no IPv4 address", name.c_str());
    set_eax(c, 0);
}
void w_inet_ntoa(X86 *c) {
    // The returned pointer is reused across calls, as the real inet_ntoa does.
    // heap_owns re-checks it because mem_init() invalidates the whole heap.
    static uint32_t buf = 0;
    if (!buf || !heap_owns(buf))
        buf = heap_alloc(32, true);
    uint32_t addr = arg(c, 0);
    char tmp[32];
    snprintf(tmp, sizeof tmp, "%u.%u.%u.%u", addr & 0xff, (addr >> 8) & 0xff, (addr >> 16) & 0xff,
             (addr >> 24) & 0xff);
    if (buf)
        gm_put_str(buf, tmp, 32);
    set_eax(c, buf);
}

// -------------------------------------------------------------------------
// Winsock with no host network. Black & White does not need the internet, so
// every call that would open or use a socket fails the way the real stack
// fails with no network adapter, and name lookups behave as an offline
// machine: gethostname reports the local name, localhost resolves, every other
// name is WSAHOST_NOT_FOUND. The byte-order and address-parsing calls need no
// network and are the real thing. This layer is a deferred fidelity gap for
// the networking task, not a silent stub; each distinct failing call is logged
// once. A call whose offline behavior cannot be justified stays a loud abort
// (see the WSA* event/overlapped entries that map to imports_unsupported).
// -------------------------------------------------------------------------
static constexpr int WSAENETDOWN_ = 10050;
static constexpr int WSAENETUNREACH_ = 10051;
static constexpr int WSAENOTSOCK_ = 10038;

// SOCKET_ERROR and INVALID_SOCKET are both all-ones, and the byte-count and
// WSAOVERLAPPED* out-parameters are left untouched: a failing call has no
// result to report.
static void wsa_call_failed(X86 *c, const char *fn, int error) {
    log_once(fn, "WS2_32/WSOCK32!%s: no network host; SOCKET_ERROR, WSAGetLastError=%d", fn, error);
    g_wsa_last_error = error;
    set_eax(c, (uint32_t)-1);
}
void w_socket(X86 *c) {
    wsa_call_failed(c, "socket", WSAENETDOWN_);
}
void w_bind(X86 *c) {
    wsa_call_failed(c, "bind", WSAENETDOWN_);
}
void w_connect(X86 *c) {
    wsa_call_failed(c, "connect", WSAENETUNREACH_);
}
void w_listen(X86 *c) {
    wsa_call_failed(c, "listen", WSAENETDOWN_);
}
void w_accept(X86 *c) {
    wsa_call_failed(c, "accept", WSAENETDOWN_);
}
void w_send(X86 *c) {
    wsa_call_failed(c, "send", WSAENETDOWN_);
}
void w_recv(X86 *c) {
    wsa_call_failed(c, "recv", WSAENETDOWN_);
}
void w_sendto(X86 *c) {
    wsa_call_failed(c, "sendto", WSAENETDOWN_);
}
void w_recvfrom(X86 *c) {
    wsa_call_failed(c, "recvfrom", WSAENETDOWN_);
}
void w_select(X86 *c) {
    wsa_call_failed(c, "select", WSAENETDOWN_);
}
void w_shutdown(X86 *c) {
    wsa_call_failed(c, "shutdown", WSAENETDOWN_);
}
void w_closesocket(X86 *c) {
    wsa_call_failed(c, "closesocket", WSAENOTSOCK_);
}
void w_ioctlsocket(X86 *c) {
    wsa_call_failed(c, "ioctlsocket", WSAENOTSOCK_);
}
void w_getsockopt(X86 *c) {
    wsa_call_failed(c, "getsockopt", WSAENOTSOCK_);
}
void w_setsockopt(X86 *c) {
    wsa_call_failed(c, "setsockopt", WSAENOTSOCK_);
}
void w_getpeername(X86 *c) {
    wsa_call_failed(c, "getpeername", WSAENOTSOCK_);
}
void w_getsockname(X86 *c) {
    wsa_call_failed(c, "getsockname", WSAENOTSOCK_);
}
void w_WSAIoctl(X86 *c) {
    wsa_call_failed(c, "WSAIoctl", WSAENETDOWN_);
}
void w_WSARecv(X86 *c) {
    wsa_call_failed(c, "WSARecv", WSAENETDOWN_);
}
void w_WSARecvFrom(X86 *c) {
    wsa_call_failed(c, "WSARecvFrom", WSAENETDOWN_);
}

// WS2_32 events are kernel events: manual-reset, created unsignalled. They work
// without a network host, so a guest that waits on one times out or is woken by
// its own WSASetEvent as on Windows. The WSA error mirrors the Win32 one.
void wsa_event_result(X86 *c, bool ok) {
    if (!ok)
        g_wsa_last_error = 6; // WSA_INVALID_HANDLE
    set_eax(c, ok);
}
void w_WSACreateEvent(X86 *c) {
    set_eax(c, win32_create_event(true, false));
}
void w_WSACloseEvent(X86 *c) {
    wsa_event_result(c, win32_close_event(arg(c, 0)));
}
void w_WSASetEvent(X86 *c) {
    wsa_event_result(c, win32_signal_event(arg(c, 0), false));
}
void w_WSAResetEvent(X86 *c) {
    wsa_event_result(c, win32_reset_event(arg(c, 0)));
}
void w_WSAWaitForMultipleEvents(X86 *c) {
    uint32_t count = arg(c, 0), events = arg(c, 1);
    bool wait_all = arg(c, 2) != 0;
    uint32_t timeout = arg(c, 3);
    bool alertable = arg(c, 4) != 0;
    if (!count || count > 64) {
        g_wsa_last_error = 87;   // WSA_INVALID_PARAMETER
        set_eax(c, 0xffffffffu); // WSA_WAIT_FAILED
        return;
    }
    if (!events || !gm_valid(events, 4 * count)) {
        g_wsa_last_error = WSAEFAULT_;
        set_eax(c, 0xffffffffu);
        return;
    }
    uint32_t handles[64];
    for (uint32_t i = 0; i < count; ++i)
        handles[i] = rd32(events + 4 * i);
    uint32_t result = guest_wait_objects_ex(c, handles, count, wait_all, timeout, alertable);
    if (result == 0xffffffffu)
        g_wsa_last_error = (int)get_last_error();
    set_eax(c, result);
}
void w_WSAGetOverlappedResult(X86 *c) {
    g_wsa_last_error = WSAENETDOWN_;
    set_eax(c, 0);
}

void w_gethostbyaddr(X86 *c) {
    log_once("WS2_32!gethostbyaddr",
             "WS2_32!gethostbyaddr: no network host; NULL, WSAGetLastError=%d", WSAHOST_NOT_FOUND_);
    g_wsa_last_error = WSAHOST_NOT_FOUND_;
    set_eax(c, 0);
}

// Pure byte-order and address utilities: no network is involved.
void w_htonl(X86 *c) {
    set_eax(c, __builtin_bswap32(arg(c, 0)));
}
void w_ntohl(X86 *c) {
    set_eax(c, __builtin_bswap32(arg(c, 0)));
}
void w_htons(X86 *c) {
    set_eax(c, (uint32_t)(uint16_t)__builtin_bswap16((uint16_t)arg(c, 0)));
}
void w_ntohs(X86 *c) {
    set_eax(c, (uint32_t)(uint16_t)__builtin_bswap16((uint16_t)arg(c, 0)));
}
void w_inet_addr(X86 *c) {
    std::string text = gm_str(arg(c, 0), 64);
    unsigned a = 0, b = 0, cc = 0, d = 0;
    char extra = 0;
    if (sscanf(text.c_str(), "%u.%u.%u.%u%c", &a, &b, &cc, &d, &extra) == 4 && a < 256 && b < 256 &&
        cc < 256 && d < 256) {
        set_eax(c, a | (b << 8) | (cc << 16) | (d << 24));
    } else {
        set_eax(c, 0xffffffffu); // INADDR_NONE
    }
}
// __WSAFDIsSet(s, fd_set*): nonzero when s is in the set. fd_set is a count
// followed by that many SOCKETs; FD_SETSIZE is 64.
void w_wsa_fd_is_set(X86 *c) {
    uint32_t s = arg(c, 0), set = arg(c, 1);
    if (!set || !gm_valid(set, 4)) {
        set_eax(c, 0);
        return;
    }
    uint32_t count = rd32(set);
    if (count > 64)
        count = 64;
    for (uint32_t i = 0; i < count; ++i) {
        if (!gm_valid(set + 4 + i * 4, 4))
            break;
        if (rd32(set + 4 + i * 4) == s) {
            set_eax(c, 1);
            return;
        }
    }
    set_eax(c, 0);
}

// -------------------------------------------------------------------------
// WINMM: clock, multimedia timers and mmio.
// -------------------------------------------------------------------------
struct MmTimer {
    uint32_t id = 0;
    uint32_t callback = 0; // a guest function, or an event handle in event mode
    uint32_t user = 0;
    uint32_t period = 0;
    uint32_t next_due = 0;
    bool periodic = false;
    bool alive = false;
    // TIME_CALLBACK_EVENT_SET / _PULSE: lpTimeProc is an event handle.
    bool event_mode = false;
    bool event_pulse = false;
};

std::map<uint32_t, MmTimer> &timers() {
    static std::map<uint32_t, MmTimer> m;
    return m;
}
uint32_t g_next_timer_id = 1;

void m_timeGetTime(X86 *c) {
    host_note_cadence("timeGetTime");
    set_eax(c, host_millis());
}

void m_timeSetEvent(X86 *c) {
    uint32_t delay = arg(c, 0), callback = arg(c, 2), user = arg(c, 3), flags = arg(c, 4);
    MmTimer t;
    t.id = g_next_timer_id++;
    t.callback = callback;
    t.user = user;
    t.period = delay ? delay : 1;
    t.periodic = (flags & 1) != 0; // TIME_PERIODIC
    t.next_due = host_millis() + t.period;
    t.alive = true;
    uint32_t cb_mode = flags & 0x30;
    t.event_mode = cb_mode != 0; // TIME_CALLBACK_EVENT_SET/_PULSE
    t.event_pulse = cb_mode == 0x20;
    timers()[t.id] = t;
    LOGV("timeSetEvent(%u ms, %s, %s=%08x) -> id %u", delay, t.periodic ? "periodic" : "one-shot",
         t.event_mode ? "event" : "callback", callback, t.id);
    set_eax(c, t.id);
}

void m_timeKillEvent(X86 *c) {
    timers().erase(arg(c, 0));
    set_eax(c, 0); // TIMERR_NOERROR
}

void m_timeGetDevCaps(X86 *c) {
    uint32_t out = arg(c, 0);
    if (!out || arg(c, 1) < 8 || !gm_valid(out, 8)) {
        set_eax(c, 97);
        return;
    }
    wr32(out, 1);
    wr32(out + 4, 1000);
    set_eax(c, 0);
}

void m_timeBeginPeriod(X86 *c) {
    set_eax(c, 0);
}
void m_timeEndPeriod(X86 *c) {
    set_eax(c, 0);
}

// mmio: the same case-insensitive file layer as CreateFileA, plus the
// memory-file form LHaudiodllR uses to hand its RIFF parser a buffer. The two
// backings share one handle table; only read/seek/write differ.
//
// Field offsets and error codes are the Windows mmsystem.h ones: MMIOINFO is
// 0x48 bytes (fccIOProc 0x04, wErrorRet 0x0c, cchBuffer 0x14, pchBuffer 0x18)
// and MMCKINFO is 0x14 bytes. The RIFF walk is what the LHaudiodllR parser
// (0x10210910) drives: mmioOpenA(NULL, MMIOINFO{'MEM '}), then
// mmioDescend/'WAVE'/FINDRIFF, 'fmt '/FINDCHUNK, mmioRead, mmioAscend, 'data'.
const uint32_t MMIO_FINDCHUNK = 0x0010;
const uint32_t MMIO_FINDRIFF = 0x0020;
const uint32_t MMIO_FINDLIST = 0x0040;
const uint32_t MMIOERR_FILENOTFOUND = 0x101;
const uint32_t MMIOERR_CANNOTOPEN = 0x103;
const uint32_t MMIOERR_CANNOTSEEK = 0x107;
const uint32_t MMIOERR_CHUNKNOTFOUND = 0x109;
const uint32_t MMIO_DIRTY = 0x10000000u;
const uint32_t FOURCC_RIFF = 0x46464952u; // 'RIFF'
const uint32_t FOURCC_LIST = 0x5453494cu; // 'LIST'
const uint32_t FOURCC_MEM = 0x204d454du;  // 'MEM '
enum {
    MMIOINFO_SIZE = 0x48,
    MMIOINFO_OFF_fccIOProc = 0x04,
    MMIOINFO_OFF_wErrorRet = 0x0c,
    MMIOINFO_OFF_cchBuffer = 0x14,
    MMIOINFO_OFF_pchBuffer = 0x18,
    MMIOINFO_OFF_pchNext = 0x1c,
    MMIOINFO_OFF_pchEndRead = 0x20,
    MMIOINFO_OFF_pchEndWrite = 0x24,
    MMCKINFO_SIZE = 0x14,
    MMCKINFO_OFF_ckid = 0x00,
    MMCKINFO_OFF_cksize = 0x04,
    MMCKINFO_OFF_fccType = 0x08,
    MMCKINFO_OFF_dwDataOffset = 0x0c,
    MMCKINFO_OFF_dwFlags = 0x10,
};

struct MmioFile {
    FILE *fp = nullptr;
    std::string path;
    bool writable = false;
    // Memory-file form: the guest buffer and the current offset into it.
    bool memory = false;
    uint32_t buffer = 0;
    uint32_t length = 0;
    uint32_t pos = 0;
};
std::map<uint32_t, MmioFile> &mmios() {
    static std::map<uint32_t, MmioFile> m;
    return m;
}
uint32_t g_next_mmio = 0x00040004;

// Reads up to n bytes at an absolute offset from either backing store into
// host memory. Returns the number of bytes read; the file position is left at
// offset + got.
uint32_t mmio_read_at(MmioFile &f, uint32_t offset, void *dst, uint32_t n) {
    if (f.memory) {
        if (offset >= f.length)
            return 0;
        uint32_t avail = f.length - offset;
        if (n > avail)
            n = avail;
        if (!n || !gm_valid(f.buffer + offset, n))
            return 0;
        memcpy(dst, g_mem + f.buffer + offset, n);
        return n;
    }
    if (!f.fp || fseek(f.fp, (long)offset, SEEK_SET) != 0)
        return 0;
    return (uint32_t)fread(dst, 1, n, f.fp);
}

void m_mmioOpenA(X86 *c) {
    uint32_t name_ptr = arg(c, 0);
    uint32_t pinfo = arg(c, 1), flags = arg(c, 2);
    bool write = (flags & 0x00000001) != 0;     // MMIO_WRITE
    bool readwrite = (flags & 0x00000002) != 0; // MMIO_READWRITE
    bool create = (flags & 0x00001000) != 0;    // MMIO_CREATE

    // Memory-file form: a null name and lpmmioinfo naming FOURCC_MEM. The
    // buffer is MMIOINFO.pchBuffer of cchBuffer. Windows fills the memory
    // cursors on the way out.
    if (!name_ptr && pinfo && gm_valid(pinfo, MMIOINFO_SIZE) &&
        rd32(pinfo + MMIOINFO_OFF_fccIOProc) == FOURCC_MEM) {
        uint32_t buf = rd32(pinfo + MMIOINFO_OFF_pchBuffer);
        uint32_t len = rd32(pinfo + MMIOINFO_OFF_cchBuffer);
        if (!buf || !len || !gm_valid(buf, len)) {
            wr32(pinfo + MMIOINFO_OFF_wErrorRet, MMIOERR_CANNOTOPEN);
            set_eax(c, 0);
            return;
        }
        MmioFile f;
        f.memory = true;
        f.buffer = buf;
        f.length = len;
        f.pos = 0;
        f.writable = write || readwrite || create;
        uint32_t h = g_next_mmio;
        g_next_mmio += 4;
        mmios()[h] = f;
        wr32(pinfo + MMIOINFO_OFF_pchNext, buf);
        wr32(pinfo + MMIOINFO_OFF_pchEndRead, buf + len);
        wr32(pinfo + MMIOINFO_OFF_pchEndWrite, buf + len);
        set_eax(c, h);
        return;
    }

    std::string name = gm_str(name_ptr, 260);
    std::string host = win32_host_path(name, write || readwrite || create);
    if (recomp_env("TRACE_FILES"))
        LOGW("file: mmioOpen \"%s\" -> \"%s\"", name.c_str(), host.c_str());
    if (host.empty()) {
        LOGV("mmioOpenA(%s): not found", name.c_str());
        if (pinfo && gm_valid(pinfo, MMIOINFO_SIZE))
            wr32(pinfo + MMIOINFO_OFF_wErrorRet, MMIOERR_FILENOTFOUND);
        set_eax(c, 0);
        return;
    }
    const char *mode = create      ? (readwrite ? "w+b" : "wb")
                       : readwrite ? "r+b"
                       : write     ? "r+b"
                                   : "rb";
    FILE *fp = fopen(host.c_str(), mode);
    if (!fp) {
        LOGV("mmioOpenA(%s, mode %s): open failed", name.c_str(), mode);
        if (pinfo && gm_valid(pinfo, MMIOINFO_SIZE))
            wr32(pinfo + MMIOINFO_OFF_wErrorRet, MMIOERR_CANNOTOPEN);
        set_eax(c, 0);
        return;
    }
    if (create || write || readwrite)
        win32_invalidate_dir_cache();
    MmioFile f;
    f.fp = fp;
    f.path = host;
    f.writable = write || readwrite || create;
    uint32_t h = g_next_mmio;
    g_next_mmio += 4;
    mmios()[h] = f;
    set_eax(c, h);
}

// Direct buffer access for FOURCC_MEM. All cursors remain guest addresses.
// Disk buffering and custom I/O procedures are not implemented here and retain
// the import's named unsupported diagnostic rather than reporting false success.
void m_mmioGetInfo(X86 *c) {
    uint32_t h = arg(c, 0), info = arg(c, 1);
    auto it = mmios().find(h);
    if (it == mmios().end() || !info || !gm_valid(info, MMIOINFO_SIZE) || arg(c, 2)) {
        set_eax(c, 11); // MMSYSERR_INVALPARAM
        return;
    }
    MmioFile &f = it->second;
    if (!f.memory) {
        imports_unsupported(c);
        return;
    }
    memset(g_mem + info, 0, MMIOINFO_SIZE);
    wr32(info, f.writable ? 2 : 0); // MMIO_READWRITE / MMIO_READ
    wr32(info + MMIOINFO_OFF_fccIOProc, FOURCC_MEM);
    wr32(info + MMIOINFO_OFF_cchBuffer, f.length);
    wr32(info + MMIOINFO_OFF_pchBuffer, f.buffer);
    wr32(info + MMIOINFO_OFF_pchNext, f.buffer + std::min(f.pos, f.length));
    wr32(info + MMIOINFO_OFF_pchEndRead, f.buffer + f.length);
    wr32(info + MMIOINFO_OFF_pchEndWrite, f.buffer + f.length);
    wr32(info + 0x30, f.length); // lDiskOffset: end of memory backing
    wr32(info + 0x44, h);        // hmmio
    set_eax(c, 0);
}

void m_mmioSetInfo(X86 *c) {
    auto it = mmios().find(arg(c, 0));
    uint32_t info = arg(c, 1);
    if (it == mmios().end() || !info || !gm_valid(info, MMIOINFO_SIZE) || arg(c, 2)) {
        set_eax(c, 11);
        return;
    }
    MmioFile &f = it->second;
    if (!f.memory) {
        imports_unsupported(c);
        return;
    }
    uint32_t next = rd32(info + MMIOINFO_OFF_pchNext);
    if (next < f.buffer || next - f.buffer > f.length) {
        set_eax(c, 11);
        return;
    }
    f.pos = next - f.buffer;
    set_eax(c, 0);
}

void m_mmioAdvance(X86 *c) {
    auto it = mmios().find(arg(c, 0));
    uint32_t info = arg(c, 1), flags = arg(c, 2);
    if (it == mmios().end() || (info && !gm_valid(info, MMIOINFO_SIZE)) || flags > 1) {
        set_eax(c, 11);
        return;
    }
    if (!it->second.memory || flags == 1) {
        imports_unsupported(c);
        return;
    }
    // A memory file is already wholly buffered: advancing at EOF yields no
    // new bytes, allowing the caller to detect exhaustion without wrapping.
    if (info) {
        uint32_t next = rd32(info + MMIOINFO_OFF_pchNext);
        if (next < it->second.buffer || next - it->second.buffer > it->second.length) {
            set_eax(c, 11);
            return;
        }
        it->second.pos = next - it->second.buffer;
    }
    it->second.pos = it->second.length;
    if (info) {
        // fuAdvance is MMIO_READ (zero), also valid as GetInfo's reserved flag.
        m_mmioGetInfo(c);
        return;
    }
    set_eax(c, 0);
}

void m_mmioWrite(X86 *c) {
    auto it = mmios().find(arg(c, 0));
    uint32_t buf = arg(c, 1), n = arg(c, 2);
    if (it == mmios().end() || !it->second.writable || !gm_valid(buf, n)) {
        set_eax(c, 0xffffffffu);
        return;
    }
    MmioFile &f = it->second;
    if (f.memory) {
        if (f.pos + n < f.pos || f.pos + n > f.length || !gm_valid(f.buffer + f.pos, n)) {
            set_eax(c, 0);
            return;
        }
        memcpy(g_mem + f.buffer + f.pos, g_mem + buf, n);
        f.pos += n;
        set_eax(c, n);
        return;
    }
    set_eax(c, (uint32_t)fwrite(g_mem + buf, 1, n, f.fp));
}

void m_mmioFlush(X86 *c) {
    auto it = mmios().find(arg(c, 0));
    if (it != mmios().end() && !it->second.memory)
        fflush(it->second.fp);
    set_eax(c, 0);
}

void m_mmioRead(X86 *c) {
    auto it = mmios().find(arg(c, 0));
    uint32_t buf = arg(c, 1), n = arg(c, 2);
    if (it == mmios().end() || !gm_valid(buf, n)) {
        set_eax(c, 0xffffffffu);
        return;
    }
    MmioFile &f = it->second;
    if (f.memory) {
        if (f.pos >= f.length) {
            set_eax(c, 0);
            return;
        }
        uint32_t avail = f.length - f.pos;
        if (n > avail)
            n = avail;
        if (n && gm_valid(f.buffer + f.pos, n))
            memcpy(g_mem + buf, g_mem + f.buffer + f.pos, n);
        f.pos += n;
        set_eax(c, n);
        return;
    }
    set_eax(c, (uint32_t)fread(g_mem + buf, 1, n, f.fp));
}

void m_mmioSeek(X86 *c) {
    auto it = mmios().find(arg(c, 0));
    int32_t off = (int32_t)arg(c, 1);
    uint32_t origin = arg(c, 2);
    if (it == mmios().end()) {
        set_eax(c, 0xffffffffu);
        return;
    }
    MmioFile &f = it->second;
    if (f.memory) {
        int64_t base = origin == 1 ? f.pos : origin == 2 ? (int64_t)f.length : 0;
        int64_t next = base + off;
        if (next < 0) {
            set_eax(c, 0xffffffffu);
            return;
        }
        f.pos = (uint32_t)next;
        set_eax(c, f.pos);
        return;
    }
    int whence = origin == 1 ? SEEK_CUR : origin == 2 ? SEEK_END : SEEK_SET;
    if (fseek(f.fp, off, whence) != 0) {
        set_eax(c, 0xffffffffu);
        return;
    }
    set_eax(c, (uint32_t)ftell(f.fp));
}

void m_mmioClose(X86 *c) {
    auto it = mmios().find(arg(c, 0));
    if (it != mmios().end()) {
        if (it->second.fp)
            fclose(it->second.fp);
        mmios().erase(it);
    }
    set_eax(c, 0);
}

void m_mmioSetBuffer(X86 *c) {
    set_eax(c, 0);
} // MMSYSERR_NOERROR

// mmioDescend/hmmio, lpck, lpckParent, fuDescend). Windows walks RIFF chunks:
// FINDRIFF selects a 'RIFF' whose fccType matches, FINDLIST a 'LIST',
// FINDCHUNK a ckid, and no flag takes the next chunk at the current offset.
// With a parent the search is bounded by its data; a chunk not found returns
// MMIOERR_CHUNKNOTFOUND. cksize counts the bytes after the 8-byte header and
// includes the form type for RIFF/LIST.
void m_mmioDescend(X86 *c) {
    auto it = mmios().find(arg(c, 0));
    uint32_t pck = arg(c, 1), parent = arg(c, 2), flags = arg(c, 3);
    if (it == mmios().end() || !pck || !gm_valid(pck, MMCKINFO_SIZE)) {
        set_eax(c, MMIOERR_CHUNKNOTFOUND);
        return;
    }
    MmioFile &f = it->second;
    uint32_t want_ckid = rd32(pck + MMCKINFO_OFF_ckid);
    uint32_t want_fcc = rd32(pck + MMCKINFO_OFF_fccType);
    uint32_t start = f.pos, end = f.length;
    if (parent && gm_valid(parent, MMCKINFO_SIZE)) {
        uint32_t poff = rd32(parent + MMCKINFO_OFF_dwDataOffset);
        uint32_t psize = rd32(parent + MMCKINFO_OFF_cksize);
        uint32_t pckid = rd32(parent + MMCKINFO_OFF_ckid);
        // For a RIFF/LIST parent dwDataOffset points at its form type; the
        // children begin after it. cksize includes the form type, so the
        // bound is dwDataOffset + cksize.
        start = (pckid == FOURCC_RIFF || pckid == FOURCC_LIST) ? poff + 4 : poff;
        end = poff + psize;
    }
    while (start + 8 <= end && start + 8 <= f.length) {
        uint32_t hdr[2];
        if (mmio_read_at(f, start, hdr, 8) != 8)
            break;
        uint32_t ckid = hdr[0], cksize = hdr[1];
        bool form = ckid == FOURCC_RIFF || ckid == FOURCC_LIST;
        uint32_t fcc = 0;
        if (form && mmio_read_at(f, start + 8, &fcc, 4) != 4)
            break;
        bool match;
        if (flags & MMIO_FINDRIFF)
            match = ckid == FOURCC_RIFF && fcc == want_fcc;
        else if (flags & MMIO_FINDLIST)
            match = ckid == FOURCC_LIST && fcc == want_fcc;
        else if (flags & MMIO_FINDCHUNK)
            match = ckid == want_ckid;
        else
            match = true;
        if (match) {
            wr32(pck + MMCKINFO_OFF_ckid, ckid);
            wr32(pck + MMCKINFO_OFF_cksize, cksize);
            if (form)
                wr32(pck + MMCKINFO_OFF_fccType, fcc);
            wr32(pck + MMCKINFO_OFF_dwDataOffset, start + 8);
            wr32(pck + MMCKINFO_OFF_dwFlags, 0);
            f.pos = start + 8;
            set_eax(c, 0);
            return;
        }
        uint32_t step = 8 + ((cksize + 1) & ~1u);
        if (step < 8 || start + step <= start)
            break;
        start += step;
    }
    set_eax(c, MMIOERR_CHUNKNOTFOUND);
}

// mmioAscend(hmmio, lpck, fuAscend): move to the byte after the chunk and
// clear MMIO_DIRTY. The chunk's header is 8 bytes and cksize counts the data.
void m_mmioAscend(X86 *c) {
    auto it = mmios().find(arg(c, 0));
    uint32_t pck = arg(c, 1);
    if (it == mmios().end() || !pck || !gm_valid(pck, MMCKINFO_SIZE)) {
        set_eax(c, MMIOERR_CANNOTSEEK);
        return;
    }
    uint32_t off = rd32(pck + MMCKINFO_OFF_dwDataOffset);
    uint32_t size = rd32(pck + MMCKINFO_OFF_cksize);
    it->second.pos = off + ((size + 1) & ~1u);
    wr32(pck + MMCKINFO_OFF_dwFlags, rd32(pck + MMCKINFO_OFF_dwFlags) & ~MMIO_DIRTY);
    set_eax(c, 0);
}

// ---------------------------------------------------------------------------
// MIDI out.
//
// MIDI messages are forwarded to the host synthesizer.
// ---------------------------------------------------------------------------
const uint32_t MMSYSERR_NOERROR = 0;
const uint32_t MMSYSERR_BADDEVICEID = 2;
const uint32_t MMSYSERR_INVALPARAM = 11;
const uint32_t MIDIERR_UNPREPARED = 64;

// MIDIOUTCAPSA: wMid, wPid, vDriverVersion, szPname[32], wTechnology, wVoices,
// wNotes, wChannelMask, dwSupport. Fifty-two bytes, which is the 0x34 the game
// passes.
const uint32_t MIDIOUTCAPSA_SIZE = 52;
const uint16_t MOD_SWSYNTH = 7;

// MIDIHDR: lpData, dwBufferLength, dwBytesRecorded, dwUser, dwFlags, lpNext,
// reserved, dwOffset, dwReserved[8]. Sixty-four bytes.
const uint32_t MIDIHDR_SIZE = 64;
const uint32_t MHDR_OFF_lpData = 0, MHDR_OFF_dwBufferLength = 4;
const uint32_t MHDR_OFF_dwFlags = 16;
const uint32_t MHDR_DONE = 0x1, MHDR_PREPARED = 0x2, MHDR_INQUEUE = 0x4;

// The one open device. The game opens exactly one; the handle is a token
// rather than an index so a stale one is caught rather than followed.
const uint32_t MIDI_HANDLE = 0x4d494449u; // 'MIDI'
struct MidiOut {
    bool open = false;
    bool synth = false;    // the host has a synth; false means silence
    uint32_t callback = 0; // guest function, 0 for CALLBACK_NULL
    uint32_t instance = 0;
    uint32_t flags = 0;
    uint32_t shorts = 0, sysexes = 0;
};
MidiOut &midi() {
    static MidiOut m;
    return m;
}

// Use the bundled General MIDI bank through the host resource layout.
std::string midi_soundfont_path() {
    if (!RECOMP_AUDIO_MIDI)
        return {};
    const std::string bundled = host_resource("general-midi.sf2");
    OsStat st;
    if (!bundled.empty() && os_stat(bundled.c_str(), &st) == 0 && st.is_regular)
        return bundled;
    return std::string();
}

void m_midiOutGetNumDevs(X86 *c) {
    set_eax(c, 1);
}
void m_auxGetNumDevs(X86 *c) {
    set_eax(c, 0);
} // no aux devices

void m_midiOutGetDevCapsA(X86 *c) {
    uint32_t id = arg(c, 0), out = arg(c, 1), size = arg(c, 2);
    // MIDI_MAPPER is (UINT)-1 and names the system's default device, which
    // here is the only device there is.
    if (id != 0 && id != 0xffffffffu) {
        set_eax(c, MMSYSERR_BADDEVICEID);
        return;
    }
    if (!out || !size) {
        set_eax(c, MMSYSERR_INVALPARAM);
        return;
    }
    uint32_t n = size < MIDIOUTCAPSA_SIZE ? size : MIDIOUTCAPSA_SIZE;
    if (!gm_valid(out, n)) {
        set_eax(c, MMSYSERR_INVALPARAM);
        return;
    }

    uint8_t caps[MIDIOUTCAPSA_SIZE];
    memset(caps, 0, sizeof caps);
    // The name is the whole point: the game accepts a device only if its first
    // nine characters are "SoundFont".
    const char *name = "SoundFont Synth";
    memcpy(caps + 8, name, strlen(name));
    caps[40] = (uint8_t)(MOD_SWSYNTH & 0xff); // wTechnology
    caps[41] = (uint8_t)(MOD_SWSYNTH >> 8);
    caps[42] = 32;
    caps[43] = 0; // wVoices
    caps[44] = 32;
    caps[45] = 0; // wNotes
    caps[46] = 0xff;
    caps[47] = 0xff; // wChannelMask: all sixteen
    for (uint32_t i = 0; i < n; ++i)
        wr8(out + i, caps[i]);
    set_eax(c, MMSYSERR_NOERROR);
}

// Open the supported MIDI output and publish the host synth prepared before guest startup.
// Avoid parsing sound banks while holding the guest scheduler baton, which would stall every worker.
void m_midiOutOpen(X86 *c) {
    uint32_t out = arg(c, 0), id = arg(c, 1);
    uint32_t callback = arg(c, 2), instance = arg(c, 3), flags = arg(c, 4);
    if (!out || !gm_valid(out, 4)) {
        set_eax(c, MMSYSERR_INVALPARAM);
        return;
    }
    if (id != 0 && id != 0xffffffffu) {
        set_eax(c, MMSYSERR_BADDEVICEID);
        return;
    }

    std::string sf2 = midi_soundfont_path();
    MidiOut &m = midi();

    // This runs on a guest thread, which holds the scheduler baton for the
    // whole call, so every other guest thread is stopped until it returns.
    // The host builds its synth before the guest starts and this call only
    // publishes it, so the measurement below should always read zero. It is
    // kept because it is the cheap guard against that changing: if a host ever
    // parses a bank here instead, the game freezes at the moment the music
    // starts with nothing anywhere to say why, and this turns that into a line.
    uint32_t before = host_millis();
    m.synth = host_midi_open(sf2.empty() ? nullptr : sf2.c_str()) != 0;
    uint32_t took = host_millis() - before;
    // Two milliseconds, not fifty. The expected reading is zero, so the
    // threshold has to be tight enough to catch the mistake that can actually
    // happen now - a parse moving back into the open - rather than only a
    // repeat of the one that was designed out. A twenty-millisecond parse
    // stutters every time the music starts and would sail under fifty.
    if (took > 2) {
        LOGW("midiOutOpen: the host took %u ms to open the synth, and it was "
             "holding the scheduler baton for all of it - every guest thread "
             "was stopped. This call should publish a synth that already "
             "exists; whatever it is doing belongs in host_midi_startup, "
             "before the guest runs",
             took);
    }
    // A host with no synth is not a reason to fail the open. The game has one
    // MIDI path and no fallback: refusing here loses the music and nothing
    // else, and a device that accepts messages and plays nothing is what a
    // silent build should look like.
    if (!m.synth) {
        log_once("midiOutOpen",
                 "midiOutOpen: the host has no synth%s, so the music is "
                 "accepted and not heard",
                 sf2.empty() ? " and no General MIDI bank was found" : "");
    } else {
        LOGW("midiOutOpen: SoundFont synth open on %s", sf2.c_str());
    }
    m.open = true;
    // CALLBACK_FUNCTION is 0x30000 in fdwOpen. The game passes zero, so this
    // is here to be right rather than because it is used.
    m.callback = (flags & 0x70000u) == 0x30000u ? callback : 0;
    m.instance = instance;
    m.flags = flags;
    m.shorts = m.sysexes = 0;
    wr32(out, MIDI_HANDLE);
    set_eax(c, MMSYSERR_NOERROR);
}

void m_midiOutClose(X86 *c) {
    MidiOut &m = midi();
    if (arg(c, 0) != MIDI_HANDLE || !m.open) {
        set_eax(c, MMSYSERR_INVALPARAM);
        return;
    }
    host_midi_close();
    LOGV("midiOutClose: %u short messages and %u sysexes were sent", m.shorts, m.sysexes);
    m = MidiOut();
    set_eax(c, MMSYSERR_NOERROR);
}

void m_midiOutReset(X86 *c) {
    if (arg(c, 0) != MIDI_HANDLE || !midi().open) {
        set_eax(c, MMSYSERR_INVALPARAM);
        return;
    }
    host_midi_reset();
    set_eax(c, MMSYSERR_NOERROR);
}

void m_midiOutShortMsg(X86 *c) {
    MidiOut &m = midi();
    if (arg(c, 0) != MIDI_HANDLE || !m.open) {
        set_eax(c, MMSYSERR_INVALPARAM);
        return;
    }
    ++m.shorts;
    host_midi_short(arg(c, 1));
    set_eax(c, MMSYSERR_NOERROR);
}

void m_midiOutPrepareHeader(X86 *c) {
    uint32_t hdr = arg(c, 1), size = arg(c, 2);
    if (arg(c, 0) != MIDI_HANDLE || !midi().open) {
        set_eax(c, MMSYSERR_INVALPARAM);
        return;
    }
    if (!hdr || size < MIDIHDR_SIZE || !gm_valid(hdr, MIDIHDR_SIZE)) {
        set_eax(c, MMSYSERR_INVALPARAM);
        return;
    }
    // Preparing is page-locking the buffer on Windows. There is nothing to
    // lock here, but the flag has to be set: midiOutLongMsg refuses a header
    // that does not carry it, and so does this one.
    wr32(hdr + MHDR_OFF_dwFlags, (rd32(hdr + MHDR_OFF_dwFlags) | MHDR_PREPARED) & ~MHDR_DONE);
    set_eax(c, MMSYSERR_NOERROR);
}

void m_midiOutUnprepareHeader(X86 *c) {
    uint32_t hdr = arg(c, 1), size = arg(c, 2);
    if (arg(c, 0) != MIDI_HANDLE || !midi().open) {
        set_eax(c, MMSYSERR_INVALPARAM);
        return;
    }
    if (!hdr || size < MIDIHDR_SIZE || !gm_valid(hdr, MIDIHDR_SIZE)) {
        set_eax(c, MMSYSERR_INVALPARAM);
        return;
    }
    uint32_t flags = rd32(hdr + MHDR_OFF_dwFlags);
    // A header still in the queue may not be unprepared; nothing here queues,
    // so that can only happen if the guest never let go of one.
    if (flags & MHDR_INQUEUE) {
        set_eax(c, 65 /* MIDIERR_STILLPLAYING */);
        return;
    }
    wr32(hdr + MHDR_OFF_dwFlags, flags & ~MHDR_PREPARED);
    set_eax(c, MMSYSERR_NOERROR);
}

void m_midiOutLongMsg(X86 *c) {
    uint32_t hdr = arg(c, 1), size = arg(c, 2);
    MidiOut &m = midi();
    if (arg(c, 0) != MIDI_HANDLE || !m.open) {
        set_eax(c, MMSYSERR_INVALPARAM);
        return;
    }
    if (!hdr || size < MIDIHDR_SIZE || !gm_valid(hdr, MIDIHDR_SIZE)) {
        set_eax(c, MMSYSERR_INVALPARAM);
        return;
    }
    uint32_t flags = rd32(hdr + MHDR_OFF_dwFlags);
    if (!(flags & MHDR_PREPARED)) {
        set_eax(c, MIDIERR_UNPREPARED);
        return;
    }

    uint32_t data = rd32(hdr + MHDR_OFF_lpData);
    uint32_t len = rd32(hdr + MHDR_OFF_dwBufferLength);
    if (len && (!data || !gm_valid(data, len))) {
        set_eax(c, MMSYSERR_INVALPARAM);
        return;
    }
    if (len) {
        ++m.sysexes;
        host_midi_sysex(gm_ptr(data), len);
    }

    // Sent, so it is done and out of the queue before this returns. Windows
    // sets these before the MOM_DONE callback and the guest reads them to know
    // it may reuse the buffer.
    wr32(hdr + MHDR_OFF_dwFlags, (flags | MHDR_DONE) & ~MHDR_INQUEUE);
    if (m.callback) {
        // MOM_DONE is 0x3c9. Windows calls this from its own thread; here it
        // is called from the guest thread that sent the message, which is the
        // only thread allowed to re-enter translated code.
        guest_call(c, m.callback, MIDI_HANDLE, 0x3c9, m.instance, hdr);
    }
    set_eax(c, MMSYSERR_NOERROR);
}
// The mixer API: there is no mixer device.  Every call says so with
// MMSYSERR_NODRIVER, and the count of devices is zero, so a game leaves its
// volume sliders alone rather than driving a control that is not there.
void m_mixerGetNumDevs(X86 *c) {
    set_eax(c, 0);
}
void m_mixerNoDriver(X86 *c) {
    set_eax(c, 6); // MMSYSERR_NODRIVER
}
// Wave in and out: there is no wave device either. Games that list output
// devices fall through to DirectSound, and no wave handle can ever be open.
void m_waveGetNumDevs(X86 *c) {
    set_eax(c, 0);
}
void m_waveBadDevice(X86 *c) {
    set_eax(c, MMSYSERR_BADDEVICEID);
}
void m_waveInvalidHandle(X86 *c) {
    set_eax(c, 5); // MMSYSERR_INVALHANDLE
}

// VERSION.dll: the executable's own version resource, read out of the mapped
// image. A game that shows its version asks for its own module's
// VS_VERSIONINFO, and comctl32.dll the version the manifest binds; any
// other file has none here. GetFileVersionInfoA hands the
// block over as it is in the image (UTF-16 strings). VerQueryValue walks
// the root, translation table and string paths. ANSI queries use scratch
// space after the tree so a later wide query still sees the original data.
namespace {
struct VersionResource {
    uint32_t addr = 0; // guest address of the VS_VERSIONINFO block in the image
    uint32_t size = 0;
};

VersionResource find_version_resource() {
    VersionResource r;
    std::vector<ResourceName> names;
    if (!resource_names(16, &names) || names.empty())
        return r;
    const auto &name = names.front();
    uint32_t id = name.id, temp = 0;
    if (name.is_string) {
        uint32_t units = utf16_units(name.name) + 1;
        temp = heap_alloc(units * 2);
        if (!temp)
            return r;
        gm_put_wstr(temp, name.name, units);
        id = temp;
    }
    uint32_t entry = resource_find(16, id);
    if (temp)
        heap_free(temp);
    if (entry)
        r.addr = resource_data(entry, &r.size);
    if (!r.addr)
        r.size = 0;
    return r;
}
// Reserve ANSI query scratch alongside the untouched UTF-16 resource. Both
// APIs return pointers within this caller-owned block, without a host cache.
uint32_t version_buffer_size(const VersionResource &r) {
    return ((r.size + 3) & ~3u) * 2;
}

// Whether `name` names the game's own executable: the same file however the
// caller spelled the directory.
bool names_own_executable(const std::string &name) {
    std::string leaf = name;
    size_t cut = leaf.find_last_of("\\/");
    if (cut != std::string::npos)
        leaf = leaf.substr(cut + 1);
    std::string exe = loader_exe_path();
    cut = exe.find_last_of("\\/");
    if (cut != std::string::npos)
        exe = exe.substr(cut + 1);
    if (exe.empty())
        exe = RECOMP_EXECUTABLE;
    return os_strcasecmp(leaf.c_str(), exe.c_str()) == 0;
}

// comctl32.dll's version is the one the executable's manifest binds: 6.10 when
// it depends on Microsoft.Windows.Common-Controls 6.0, else the 5.82 Windows
// loads for everything else. The VCL turns its themed painting on by this
// number, so the answer follows the manifest, not the host.
bool manifest_binds_common_controls_6() {
    std::vector<ResourceName> names;
    if (!resource_names(24, &names)) // RT_MANIFEST
        return false;
    const std::string assembly = "Microsoft.Windows.Common-Controls";
    for (const auto &name : names) {
        if (name.is_string)
            continue;
        uint32_t size = 0, entry = resource_find(24, name.id);
        uint32_t at = entry ? resource_data(entry, &size) : 0;
        if (!at || !size || !gm_valid(at, size))
            continue;
        const std::string text(reinterpret_cast<const char *>(g_mem + at), size);
        for (size_t hit = text.find(assembly); hit != std::string::npos;
             hit = text.find(assembly, hit + 1)) {
            size_t open = text.rfind('<', hit), close = text.find('>', hit);
            if (open != std::string::npos && close != std::string::npos &&
                text.substr(open, close - open).find("version=\"6.") != std::string::npos)
                return true;
        }
    }
    return false;
}

// A VS_VERSIONINFO holding only its VS_FIXEDFILEINFO, which is what a version
// check reads, in one block kept for the life of the process.
VersionResource comctl32_version_resource() {
    static uint32_t block = 0;
    if (!block)
        block = heap_alloc(92, true);
    if (!block)
        return VersionResource();
    const uint32_t file_ms = manifest_binds_common_controls_6() ? 0x0006000au : 0x00050052u;
    wr16(block, 92);     // wLength
    wr16(block + 2, 52); // wValueLength: sizeof(VS_FIXEDFILEINFO)
    wr16(block + 4, 0);  // wType: binary
    gm_put_wstr(block + 6, "VS_VERSION_INFO", 16);
    wr16(block + 38, 0); // padding to the value
    const uint32_t fixed[13] = {0xfeef04bdu, 0x00010000u, file_ms, 0x4a610456u, file_ms,
                                0x4a610456u, 0x3fu,       0,       0x00040004u, 2, // VFT_DLL
                                0,           0,           0};
    for (uint32_t i = 0; i < 13; ++i)
        wr32(block + 40 + 4 * i, fixed[i]);
    VersionResource r;
    r.addr = block;
    r.size = 92;
    return r;
}

// The version resource GetFileVersionInfo reads for `name`.
VersionResource version_resource_named(const std::string &name) {
    if (names_own_executable(name))
        return find_version_resource();
    size_t cut = name.find_last_of("\\/");
    const std::string leaf = cut == std::string::npos ? name : name.substr(cut + 1);
    if (os_strcasecmp(leaf.c_str(), "comctl32.dll") == 0)
        return comctl32_version_resource();
    return VersionResource();
}

// One block of a VS_VERSIONINFO tree, as it lies in guest memory.
struct VerBlock {
    uint32_t at = 0, length = 0, value_length = 0, type = 0;
    std::string key; // narrowed
    uint32_t value = 0, children = 0, end = 0;
};

bool read_block(uint32_t at, uint32_t limit, VerBlock *b) {
    if (at > limit || limit > GUEST_SIZE || !gm_valid(at, 6) || limit - at < 6)
        return false;
    b->at = at;
    b->length = rd16(at);
    b->value_length = rd16(at + 2);
    b->type = rd16(at + 4);
    if (b->length < 6 || at + b->length > limit)
        return false;
    b->end = at + b->length;
    uint32_t p = at + 6;
    bool terminated = false;
    b->key.clear();
    while (p + 2 <= b->end) {
        uint16_t w = rd16(p);
        p += 2;
        if (!w) {
            terminated = true;
            break;
        }
        b->key.push_back(w < 256 ? (char)w : '?');
    }
    if (!terminated)
        return false;
    p = (p + 3) & ~3u;
    b->value = p;
    uint32_t vbytes = b->type == 1 ? b->value_length * 2 : b->value_length;
    if (vbytes && (p > b->end || vbytes > b->end - p))
        return false;
    b->children = (p + vbytes + 3) & ~3u;
    return true;
}

bool find_child(const VerBlock &parent, const char *key, VerBlock *out) {
    for (uint32_t p = parent.children; p < parent.end;) {
        VerBlock c;
        if (!read_block(p, parent.end, &c))
            return false;
        if (os_strcasecmp(c.key.c_str(), key) == 0) {
            *out = c;
            return true;
        }
        p = (c.end + 3) & ~3u;
    }
    return false;
}

} // namespace

void version_size(X86 *c, const std::string &name) {
    uint32_t handle_out = arg(c, 1);
    if (handle_out && gm_valid(handle_out, 4))
        wr32(handle_out, 0);
    VersionResource r = version_resource_named(name);
    if (!r.size) {
        set_last_error(1813); // ERROR_RESOURCE_TYPE_NOT_FOUND
        set_eax(c, 0);
        return;
    }
    set_last_error(0);
    set_eax(c, version_buffer_size(r));
}

// GetFileVersionInfoA(name, handle, len, data)
void version_info(X86 *c, const std::string &name) {
    uint32_t len = arg(c, 2), data = arg(c, 3);
    VersionResource r = version_resource_named(name);
    if (!r.size) {
        set_last_error(1813);
        set_eax(c, 0);
        return;
    }
    if (!data || !gm_valid(data, len) || len < version_buffer_size(r)) {
        set_last_error(122); // ERROR_INSUFFICIENT_BUFFER
        set_eax(c, 0);
        return;
    }
    memcpy(g_mem + data, g_mem + r.addr, r.size);
    memset(g_mem + data + r.size, 0, version_buffer_size(r) - r.size);
    set_eax(c, 1);
}

// VerQueryValueA(block, subblock, ppBuffer, puLen)
void version_query(X86 *c, bool wide) {
    uint32_t block = arg(c, 0), sub = arg(c, 1), pbuf = arg(c, 2), plen = arg(c, 3);
    if (!block || !gm_valid(block, 6) || !sub || !pbuf || !gm_valid(pbuf, 4)) {
        set_eax(c, 0);
        return;
    }
    std::string path = wide ? gm_wstr(sub) : gm_str(sub);
    VerBlock root;
    if (!read_block(block, block + rd16(block), &root)) {
        set_eax(c, 0);
        return;
    }
    // Split on backslashes; a leading one names the root.
    std::vector<std::string> parts;
    std::string cur;
    for (char ch : path) {
        if (ch == '\\' || ch == '/') {
            if (!cur.empty())
                parts.push_back(cur);
            cur.clear();
        } else
            cur.push_back(ch);
    }
    if (!cur.empty())
        parts.push_back(cur);
    VerBlock b = root;
    for (const std::string &part : parts) {
        VerBlock child;
        if (!find_child(b, part.c_str(), &child)) {
            set_eax(c, 0);
            return;
        }
        b = child;
    }
    uint32_t out_len = b.value_length;
    if (parts.empty()) {
        // VS_FIXEDFILEINFO, whatever the header's own length says.
        out_len = b.value_length ? b.value_length : 0x34;
    } else if (b.type == 1 && !wide) {
        uint32_t ansi = block + ((root.length + 3) & ~3u) + (b.value - block) / 2;
        if (!gm_valid(ansi, b.value_length)) {
            set_eax(c, 0);
            return;
        }
        for (uint32_t i = 0; i < b.value_length; ++i) {
            uint16_t w = rd16(b.value + i * 2);
            wr8(ansi + i, uint8_t(w < 256 ? w : '?'));
        }
        b.value = ansi;
    }
    wr32(pbuf, b.value);
    if (plen && gm_valid(plen, 4))
        wr32(plen, out_len);
    set_eax(c, 1);
}

void v_GetFileVersionInfoSizeA(X86 *c) {
    version_size(c, gm_str(arg(c, 0)));
}
void v_GetFileVersionInfoSizeW(X86 *c) {
    version_size(c, gm_wstr(arg(c, 0)));
}
void v_GetFileVersionInfoA(X86 *c) {
    version_info(c, gm_str(arg(c, 0)));
}
void v_GetFileVersionInfoW(X86 *c) {
    version_info(c, gm_wstr(arg(c, 0)));
}
void v_VerQueryValueA(X86 *c) {
    version_query(c, false);
}
void v_VerQueryValueW(X86 *c) {
    version_query(c, true);
}

// mciGetErrorStringA(error, buffer, length): the one MCI answer given above.
void m_mciGetErrorStringA(X86 *c) {
    uint32_t buf = arg(c, 1), len = arg(c, 2);
    if (!buf || !len) {
        set_eax(c, 0);
        return;
    }
    gm_put_str(buf, "The specified device is not installed on the system.", len);
    set_eax(c, 1);
}

void m_mciSendCommandA(X86 *c) {
    log_once("mciSendCommandA", "mciSendCommandA(%08x): MCI is not implemented", arg(c, 1));
    set_eax(c, 266); // MCIERR_DEVICE_NOT_INSTALLED
}

} // namespace

// The bank's path, for a host that loads it before the guest starts. Inside
// midiOutOpen would be a guest thread holding the scheduler baton, and a
// SoundFont parse there stops every other guest thread for its duration.
std::string win32_midi_soundfont_path() {
    return midi_soundfont_path();
}

// ---------------------------------------------------------------------------
// The synth, when there is no host to provide one. Weak, so a host's own
// definitions win; a build without one accepts every message and plays
// nothing, which is what the shims above already report.
// ---------------------------------------------------------------------------
extern "C" {
__attribute__((weak)) int host_midi_open(const char *) {
    return 0;
}
__attribute__((weak)) void host_midi_short(uint32_t) {}
__attribute__((weak)) void host_midi_sysex(const void *, uint32_t) {}
__attribute__((weak)) void host_midi_reset(void) {}
__attribute__((weak)) void host_midi_close(void) {}
}

// More than one subsystem needs the once-a-frame seam, so this is a list and
// not a slot.
//
// It was a slot, and the audio shim held it, because audio was the first thing
// that needed a per-frame tick. The display then needed the same seam - this
// is the only place the guest's own message loop reaches once a frame - and
// the shape of the tree invited putting a display call inside the audio pump.
// That would have made the display's frame boundary a detail of qmixer.cpp,
// where nobody looking for it would find it, and would have made removing the
// audio pump silently remove the display's.
//
// Four is more than the subsystems that exist; registering more is a
// programming error rather than a condition to handle at runtime, so it says
// so and drops the extra rather than growing.
namespace {
const int kMaxFramePumps = 4;
void (*g_frame_pumps[kMaxFramePumps])(X86 *) = {nullptr, nullptr, nullptr, nullptr};
int g_frame_pump_count = 0;
} // namespace

void host_set_frame_pump(void (*fn)(X86 *c)) {
    if (!fn) {
        g_frame_pump_count = 0;
        return;
    } // clearing takes them all
    for (int i = 0; i < g_frame_pump_count; ++i)
        if (g_frame_pumps[i] == fn)
            return; // registering twice is once
    if (g_frame_pump_count >= kMaxFramePumps) {
        log_once("frame-pump-full",
                 "more than %d frame pumps registered; the last is dropped and "
                 "its subsystem will not tick",
                 kMaxFramePumps);
        return;
    }
    g_frame_pumps[g_frame_pump_count++] = fn;
}

void host_pump_timers(X86 *c) {
    // Before the timers, because a shim registered here may want to start a
    // sound that a timer callback then asks about.
    for (int i = 0; i < g_frame_pump_count; ++i)
        g_frame_pumps[i](c);

    if (timers().empty())
        return;
    uint32_t now = host_millis();
    std::vector<uint32_t> due;
    for (auto &kv : timers())
        if (kv.second.alive && (int32_t)(now - kv.second.next_due) >= 0)
            due.push_back(kv.first);
    for (uint32_t id : due) {
        auto it = timers().find(id);
        if (it == timers().end())
            continue;
        MmTimer t = it->second;
        if (t.periodic)
            it->second.next_due = now + t.period;
        else
            timers().erase(it);
        if (!t.callback)
            continue;
        if (t.event_mode) {
            if (!win32_signal_event(t.callback, t.event_pulse))
                log_once("timer-event-handle",
                         "timeSetEvent timer %u refers to handle %08x, which is not an event", t.id,
                         t.callback);
        } else {
            host_note_cadence("mm_callback");
            uint32_t args[5] = {t.id, 0, t.user, 0, 0};
            guest_call(c, t.callback, args, 5);
        }
    }
}

// A stable name for the virtual Windows account, independent of host identity.
// The size includes NUL on both success and the insufficient-buffer path.
static void a_GetUserNameA(X86 *c) {
    constexpr char name[] = "Player";
    uint32_t out = arg(c, 0), size = arg(c, 1);
    set_eax(c, 0);
    if (!size || !gm_valid(size, 4)) {
        set_last_error(87);
        return;
    }
    uint32_t capacity = rd32(size);
    wr32(size, sizeof name);
    if (capacity < sizeof name) {
        set_last_error(122);
        return;
    }
    if (!out || !gm_valid(out, sizeof name)) {
        set_last_error(87);
        return;
    }
    memcpy(g_mem + out, name, sizeof name);
    set_eax(c, 1);
}

// The wide spelling, which LHNetCreateDefaultProfile uses: it passes a 97-TCHAR
// stack-local buffer and aborts profile creation if this returns zero. The
// reported size is in TCHARs (7 including the NUL), not bytes, and the buffer
// receives UTF-16. Identical name and error conventions as the ANSI form.
static void a_GetUserNameW(X86 *c) {
    constexpr char16_t name[] = u"Player";
    constexpr uint32_t required = sizeof name / sizeof name[0];
    uint32_t out = arg(c, 0), size = arg(c, 1);
    set_eax(c, 0);
    if (!size || !gm_valid(size, 4)) {
        set_last_error(87);
        return;
    }
    uint32_t capacity = rd32(size);
    wr32(size, required);
    if (capacity < required) {
        set_last_error(122);
        return;
    }
    if (!out || !gm_valid(out, required * 2)) {
        set_last_error(87);
        return;
    }
    memcpy(g_mem + out, name, required * 2);
    set_eax(c, 1);
}

static void a_AllocateAndInitializeSid(X86 *c) {
    uint32_t authority = arg(c, 0), count = arg(c, 1) & 0xff, out = arg(c, 10);
    set_eax(c, 0);
    if (!out || !gm_valid(out, 4) || !authority || !gm_valid(authority, 6) || !count || count > 8) {
        set_last_error(87);
        return;
    }
    uint32_t sid = heap_alloc(8 + count * 4, true);
    if (!sid) {
        set_last_error(8);
        return;
    }
    wr8(sid, 1);
    wr8(sid + 1, (uint8_t)count);
    memcpy(gm_ptr(sid + 2), gm_ptr(authority), 6);
    for (uint32_t i = 0; i < count; ++i)
        wr32(sid + 8 + i * 4, arg(c, 2 + (int)i));
    wr32(out, sid);
    set_eax(c, 1);
}

static void a_FreeSid(X86 *c) {
    uint32_t sid = arg(c, 0);
    if (sid && heap_owns(sid))
        heap_free(sid);
    set_eax(c, 0);
}

// The runtime has no Windows token; report a valid token that is not a member.
static void a_CheckTokenMembership(X86 *c) {
    uint32_t sid = arg(c, 1), out = arg(c, 2);
    set_eax(c, 0);
    if (!sid || !gm_valid(sid, 8) || !out || !gm_valid(out, 4)) {
        set_last_error(87);
        return;
    }
    wr32(out, 0);
    set_eax(c, 1);
}

static void a_CryptAcquireContextA(X86 *c) {
    uint32_t out = arg(c, 0);
    set_eax(c, 0);
    if (!out || !gm_valid(out, 4)) {
        set_last_error(87);
        return;
    }
    wr32(out, 0x43525950);
    set_eax(c, 1);
}

static void a_CryptGenRandom(X86 *c) {
    uint32_t len = arg(c, 1), buf = arg(c, 2);
    set_eax(c, 0);
    if (!buf || !gm_valid(buf, len)) {
        set_last_error(87);
        return;
    }
    try {
        std::random_device entropy;
        for (uint32_t i = 0; i < len; ++i)
            wr8(buf + i, (uint8_t)entropy());
    } catch (...) {
        set_last_error(87);
        return;
    }
    set_eax(c, 1);
}

static void a_CryptReleaseContext(X86 *c) {
    (void)c;
    set_eax(c, 1);
}

// No AVIFile codec adapter is installed yet. Report missing codec support at
// open instead of inventing a successful file interface and corrupting the
// caller's stack. The game can follow its normal missing-video path.
static void avi_noop(X86 *c) {
    set_eax(c, 0);
}
static void avi_open_unsupported(X86 *c) {
    uint32_t out = arg(c, 0);
    if (out && gm_valid(out, 4))
        wr32(out, 0);
    set_eax(c, 0x80040154u); // REGDB_E_CLASSNOTREG
}
static void avi_bad_handle(X86 *c) {
    set_eax(c, 0x8004406cu);
}
static void avi_no_sample(X86 *c) {
    set_eax(c, 0xffffffffu);
}

const ImportShim g_misc_shims[] = {
    // Published Win32 stdcall signatures; unsupported calls retain named faults.
    {"comdlg32.dll", "GetOpenFileNameA", 1, imports_unsupported},
    {"GDI32.dll", "GetTextFaceA", 3, imports_unsupported},
    // ADVAPI32
    {"ADVAPI32.dll", "GetUserNameA", 2, a_GetUserNameA},
    {"ADVAPI32.dll", "GetUserNameW", 2, a_GetUserNameW},
    {"ADVAPI32.dll", "AllocateAndInitializeSid", 11, a_AllocateAndInitializeSid},
    {"ADVAPI32.dll", "FreeSid", 1, a_FreeSid},
    {"ADVAPI32.dll", "CheckTokenMembership", 3, a_CheckTokenMembership},
    {"ADVAPI32.dll", "CryptAcquireContextA", 5, a_CryptAcquireContextA},
    {"ADVAPI32.dll", "CryptGenRandom", 3, a_CryptGenRandom},
    {"ADVAPI32.dll", "CryptReleaseContext", 2, a_CryptReleaseContext},
    {"ADVAPI32.dll", "RegOpenKeyA", 3, a_RegOpenKeyA},
    {"ADVAPI32.dll", "RegOpenKeyExA", 5, a_RegOpenKeyExA},
    {"ADVAPI32.dll", "RegCreateKeyExA", 9, a_RegCreateKeyExA},
    {"ADVAPI32.dll", "RegCreateKeyA", 3, a_RegCreateKeyA},
    {"ADVAPI32.dll", "RegCloseKey", 1, a_RegCloseKey},
    {"ADVAPI32.dll", "RegQueryValueExA", 6, a_RegQueryValueExA},
    {"ADVAPI32.dll", "RegQueryValueA", 4, a_RegQueryValueA},
    {"ADVAPI32.dll", "RegSetValueExA", 6, a_RegSetValueExA},
    {"ADVAPI32.dll", "RegOpenKeyExW", 5, a_RegOpenKeyExW},
    {"ADVAPI32.dll", "RegCreateKeyExW", 9, a_RegCreateKeyExW},
    {"ADVAPI32.dll", "RegQueryValueExW", 6, a_RegQueryValueExW},
    {"ADVAPI32.dll", "RegSetValueExW", 6, a_RegSetValueExW},
    {"ADVAPI32.dll", "RegEnumKeyExW", 8, a_RegEnumKeyExW},
    {"ADVAPI32.dll", "RegEnumKeyA", 4, a_RegEnumKeyA},
    {"ADVAPI32.dll", "RegEnumValueA", 8, a_RegEnumValueA},
    {"ADVAPI32.dll", "RegEnumValueW", 8, a_RegEnumValueW},
    {"ADVAPI32.dll", "RegQueryInfoKeyA", 12, a_RegQueryInfoKeyA},
    {"ADVAPI32.dll", "RegQueryInfoKeyW", 12, a_RegQueryInfoKeyW},
    {"ADVAPI32.dll", "RegDeleteKeyA", 2, a_RegDeleteKeyA},
    {"ADVAPI32.dll", "RegDeleteKeyW", 2, a_RegDeleteKeyW},
    {"ADVAPI32.dll", "RegDeleteValueW", 2, a_RegDeleteValueW},
    {"ADVAPI32.dll", "RegFlushKey", 1, a_RegFlushKey},
    // Thread tokens have no runtime equivalent; fail with the import named.
    {"ADVAPI32.dll", "OpenThreadToken", 4, imports_unsupported},
    {"ADVAPI32.dll", "RevertToSelf", 0, imports_unsupported},
    {"ADVAPI32.dll", "SetThreadToken", 2, imports_unsupported},
    {"ADVAPI32.dll", "RegConnectRegistryW", 3, a_RegDenied},
    {"ADVAPI32.dll", "RegLoadKeyW", 3, a_RegDenied},
    {"ADVAPI32.dll", "RegUnLoadKeyW", 2, a_RegDenied},
    {"ADVAPI32.dll", "RegSaveKeyW", 3, a_RegDenied},
    {"ADVAPI32.dll", "RegRestoreKeyW", 3, a_RegDenied},
    {"ADVAPI32.dll", "RegReplaceKeyW", 4, a_RegDenied},
    // SHELL32
    {"SHELL32.dll", "ShellExecuteA", 6, s_ShellExecuteA},
    {"SHELL32.dll", "SHGetSpecialFolderPathA", 4, s_SHGetSpecialFolderPath<false>},
    {"SHELL32.dll", "SHGetSpecialFolderPathW", 4, s_SHGetSpecialFolderPath<true>},
    {"SHFOLDER.dll", "SHGetFolderPathA", 5, s_SHGetFolderPathA},
    // ole32
    {"ole32.dll", "CoInitialize", 1, o_CoInitialize},
    {"ole32.dll", "CoCreateGuid", 1, o_CoCreateGuid},
    {"ole32.dll", "OleInitialize", 1, o_CoInitialize},
    {"ole32.dll", "OleUninitialize", 0, o_CoUninitialize},
    {"ole32.dll", "CoInitializeEx", 2, o_CoInitialize},
    {"ole32.dll", "CoLockObjectExternal", 3, o_CoLockObjectExternal},
    {"ole32.dll", "RegisterDragDrop", 2, o_RegisterDragDrop},
    {"ole32.dll", "RevokeDragDrop", 1, o_RevokeDragDrop},
    {"ole32.dll", "CoTaskMemAlloc", 1, o_CoTaskMemAlloc},
    {"ole32.dll", "CoTaskMemFree", 1, o_CoTaskMemFree},
    {"ole32.dll", "StringFromCLSID", 2, o_StringFromCLSID},
    {"ole32.dll", "IsEqualGUID", 2, o_IsEqualGUID},
    {"ole32.dll", "CoFileTimeToDosDateTime", 3, o_CoFileTimeToDosDateTime},
    {"ole32.dll", "CoUninitialize", 0, o_CoUninitialize},
    {"ole32.dll", "PropVariantClear", 1, o_PropVariantClear},
    {"ole32.dll", "PropVariantCopy", 2, o_PropVariantCopy},
    // IMM32
    {"IMM32.dll", "ImmIsUIMessageA", 4, i_ImmIsUIMessageA},
    {"IMM32.dll", "ImmGetContext", 1, i_ImmGetContext},
    {"IMM32.dll", "ImmReleaseContext", 2, i_ImmReleaseContext},
    {"IMM32.dll", "ImmGetOpenStatus", 1, i_ImmGetOpenStatus},
    {"IMM32.dll", "ImmSetOpenStatus", 2, i_ImmSetOpenStatus},
    {"IMM32.dll", "ImmGetCompositionStringA", 4, i_ImmGetCompositionStringA},
    {"IMM32.dll", "ImmGetCandidateListA", 4, i_ImmGetCandidateListA},
    {"IMM32.dll", "ImmSetCompositionWindow", 2, i_ImmSetCompositionWindow},
    {"IMM32.dll", "ImmAssociateContext", 2, i_ImmAssociateContext},
    {"IMM32.dll", "ImmCreateContext", 0, i_ImmCreateContext},
    {"IMM32.dll", "ImmDestroyContext", 1, i_ImmDestroyContext},
    {"IMM32.dll", "ImmGetProperty", 2, i_ImmGetProperty},
    {"IMM32.dll", "ImmGetDescriptionA", 3, i_ImmGetDescriptionA},
    {"IMM32.dll", "ImmDisableTextFrameService", 1, i_ImmDisableTextFrameService},
    // WSOCK32. D3DPopTB.exe imports these five by ordinal, so each shim is
    // registered under both the ordinal the IAT uses and the documented name
    // (which is what GetProcAddress would ask for).
    {"WSOCK32.dll", "WSAStartup", 2, w_WSAStartup},
    {"WSOCK32.dll", "ord115", 2, w_WSAStartup},
    {"WSOCK32.dll", "WSACleanup", 0, w_WSACleanup},
    {"WSOCK32.dll", "ord116", 0, w_WSACleanup},
    {"WSOCK32.dll", "gethostname", 2, w_gethostname},
    {"WSOCK32.dll", "ord57", 2, w_gethostname},
    {"WSOCK32.dll", "gethostbyname", 1, w_gethostbyname},
    {"WSOCK32.dll", "ord52", 1, w_gethostbyname},
    {"WSOCK32.dll", "inet_ntoa", 1, w_inet_ntoa},
    {"WSOCK32.dll", "ord11", 1, w_inet_ntoa},
    {"WSOCK32.dll", "WSAGetLastError", 0, w_WSAGetLastError},
    {"WSOCK32.dll", "ord111", 0, w_WSAGetLastError},
    {"WSOCK32.dll", "WSASetLastError", 1, w_WSASetLastError},
    {"WSOCK32.dll", "ord112", 1, w_WSASetLastError},
    // The rest of the Winsock 1.1 surface follows the same offline rules as
    // WS2_32 above (same ordinals, same handlers).
    {"WSOCK32.dll", "gethostbyaddr", 3, w_gethostbyaddr},
    {"WSOCK32.dll", "ord51", 3, w_gethostbyaddr},
    {"WSOCK32.dll", "htonl", 1, w_htonl},
    {"WSOCK32.dll", "ord8", 1, w_htonl},
    {"WSOCK32.dll", "htons", 1, w_htons},
    {"WSOCK32.dll", "ord9", 1, w_htons},
    {"WSOCK32.dll", "inet_addr", 1, w_inet_addr},
    {"WSOCK32.dll", "ord10", 1, w_inet_addr},
    {"WSOCK32.dll", "ntohl", 1, w_ntohl},
    {"WSOCK32.dll", "ord14", 1, w_ntohl},
    {"WSOCK32.dll", "ntohs", 1, w_ntohs},
    {"WSOCK32.dll", "ord15", 1, w_ntohs},
    {"WSOCK32.dll", "socket", 3, w_socket},
    {"WSOCK32.dll", "ord23", 3, w_socket},
    {"WSOCK32.dll", "bind", 3, w_bind},
    {"WSOCK32.dll", "ord2", 3, w_bind},
    {"WSOCK32.dll", "connect", 3, w_connect},
    {"WSOCK32.dll", "ord4", 3, w_connect},
    {"WSOCK32.dll", "listen", 2, w_listen},
    {"WSOCK32.dll", "ord13", 2, w_listen},
    {"WSOCK32.dll", "accept", 3, w_accept},
    {"WSOCK32.dll", "ord1", 3, w_accept},
    {"WSOCK32.dll", "send", 4, w_send},
    {"WSOCK32.dll", "ord19", 4, w_send},
    {"WSOCK32.dll", "recv", 4, w_recv},
    {"WSOCK32.dll", "ord16", 4, w_recv},
    {"WSOCK32.dll", "sendto", 6, w_sendto},
    {"WSOCK32.dll", "ord20", 6, w_sendto},
    {"WSOCK32.dll", "recvfrom", 6, w_recvfrom},
    {"WSOCK32.dll", "ord17", 6, w_recvfrom},
    {"WSOCK32.dll", "select", 5, w_select},
    {"WSOCK32.dll", "ord18", 5, w_select},
    {"WSOCK32.dll", "shutdown", 2, w_shutdown},
    {"WSOCK32.dll", "ord22", 2, w_shutdown},
    {"WSOCK32.dll", "closesocket", 1, w_closesocket},
    {"WSOCK32.dll", "ord3", 1, w_closesocket},
    {"WSOCK32.dll", "ioctlsocket", 3, w_ioctlsocket},
    {"WSOCK32.dll", "ord12", 3, w_ioctlsocket},
    {"WSOCK32.dll", "getsockopt", 5, w_getsockopt},
    {"WSOCK32.dll", "ord7", 5, w_getsockopt},
    {"WSOCK32.dll", "setsockopt", 5, w_setsockopt},
    {"WSOCK32.dll", "ord21", 5, w_setsockopt},
    {"WSOCK32.dll", "getpeername", 3, w_getpeername},
    {"WSOCK32.dll", "ord5", 3, w_getpeername},
    {"WSOCK32.dll", "getsockname", 3, w_getsockname},
    {"WSOCK32.dll", "ord6", 3, w_getsockname},
    {"WSOCK32.dll", "__WSAFDIsSet", 2, w_wsa_fd_is_set},
    {"WSOCK32.dll", "ord151", 2, w_wsa_fd_is_set},
    // WINMM: implemented
    {"WINMM.dll", "timeGetTime", 0, m_timeGetTime},
    {"WINMM.dll", "timeGetDevCaps", 2, m_timeGetDevCaps},
    {"_INMM.dll", "timeGetTime", 0, m_timeGetTime},
    {"_INMM.dll", "timeGetDevCaps", 2, m_timeGetDevCaps},
    {"_INMM.dll", "timeBeginPeriod", 1, m_timeBeginPeriod},
    {"_INMM.dll", "timeEndPeriod", 1, m_timeEndPeriod},
    {"WINMM.dll", "timeSetEvent", 5, m_timeSetEvent},
    {"WINMM.dll", "timeKillEvent", 1, m_timeKillEvent},
    {"WINMM.dll", "timeBeginPeriod", 1, m_timeBeginPeriod},
    {"WINMM.dll", "timeEndPeriod", 1, m_timeEndPeriod},
    {"WINMM.dll", "mmioOpenA", 3, m_mmioOpenA},
    {"WINMM.dll", "mmioGetInfo", 3, m_mmioGetInfo},
    {"WINMM.dll", "mmioSetInfo", 3, m_mmioSetInfo},
    {"WINMM.dll", "mmioAdvance", 3, m_mmioAdvance},
    {"WINMM.dll", "mmioRead", 3, m_mmioRead},
    {"WINMM.dll", "mmioSeek", 3, m_mmioSeek},
    {"WINMM.dll", "mmioDescend", 4, m_mmioDescend},
    {"WINMM.dll", "mmioAscend", 3, m_mmioAscend},
    {"WINMM.dll", "mmioClose", 2, m_mmioClose},
    {"WINMM.dll", "mmioSetBuffer", 4, m_mmioSetBuffer},
    // Not imported by this EXE, but mmio is only coherent with both halves.
    {"WINMM.dll", "mmioWrite", 3, m_mmioWrite},
    {"WINMM.dll", "mmioFlush", 2, m_mmioFlush},
    {"WINMM.dll", "midiOutGetNumDevs", 0, m_midiOutGetNumDevs},
    {"WINMM.dll", "auxGetNumDevs", 0, m_auxGetNumDevs},
    {"WINMM.dll", "midiOutGetDevCapsA", 3, m_midiOutGetDevCapsA},
    {"WINMM.dll", "midiOutOpen", 5, m_midiOutOpen},
    {"WINMM.dll", "midiOutClose", 1, m_midiOutClose},
    {"WINMM.dll", "midiOutReset", 1, m_midiOutReset},
    {"WINMM.dll", "midiOutShortMsg", 2, m_midiOutShortMsg},
    {"WINMM.dll", "midiOutLongMsg", 3, m_midiOutLongMsg},
    {"WINMM.dll", "midiOutPrepareHeader", 3, m_midiOutPrepareHeader},
    {"WINMM.dll", "midiOutUnprepareHeader", 3, m_midiOutUnprepareHeader},
    {"WINMM.dll", "mciSendCommandA", 4, m_mciSendCommandA},
    // _inmm exposes the same WinMM ABI. Until CD-file playback is modeled,
    // report the actual unavailable MCI device with its proper stack cleanup.
    {"_INMM.dll", "mciSendCommandA", 4, m_mciSendCommandA},
    {"WINMM.dll", "mciGetErrorStringA", 3, m_mciGetErrorStringA},
    {"WINMM.dll", "mixerGetNumDevs", 0, m_mixerGetNumDevs},
    {"WINMM.dll", "mixerOpen", 6, m_mixerNoDriver},
    {"WINMM.dll", "mixerClose", 1, m_mixerNoDriver},
    {"WINMM.dll", "mixerGetDevCapsA", 3, m_mixerNoDriver},
    {"WINMM.dll", "mixerGetLineInfoA", 3, m_mixerNoDriver},
    {"WINMM.dll", "mixerGetLineControlsA", 3, m_mixerNoDriver},
    {"WINMM.dll", "mixerGetControlDetailsA", 3, m_mixerNoDriver},
    {"WINMM.dll", "mixerSetControlDetails", 3, m_mixerNoDriver},
    {"VERSION.dll", "GetFileVersionInfoSizeA", 2, v_GetFileVersionInfoSizeA},
    {"VERSION.dll", "GetFileVersionInfoA", 4, v_GetFileVersionInfoA},
    {"VERSION.dll", "VerQueryValueA", 4, v_VerQueryValueA},
    {"VERSION.dll", "GetFileVersionInfoSizeW", 2, v_GetFileVersionInfoSizeW},
    {"VERSION.dll", "GetFileVersionInfoW", 4, v_GetFileVersionInfoW},
    {"VERSION.dll", "VerQueryValueW", 4, v_VerQueryValueW},
    // WINMM: logging-only, correct stdcall pop counts so the guest stack stays
    // balanced. MIDI and aux output belong to the audio task.
    {"WINMM.dll", "auxGetDevCapsA", 3, nullptr},
    {"WINMM.dll", "auxGetVolume", 2, nullptr},
    {"WINMM.dll", "auxSetVolume", 2, nullptr},
    // DirectX and third-party DLLs are owned by later tasks; these entries only
    // record the callee's pop count so an early call cannot unbalance the stack.
    // Direct3D 9, D3DX 9 and DirectInput 8: a shader-era game's graphics and
    // input DLLs. The kit models neither yet, so these record the callee's
    // stdcall pop count and nothing else. Without a count the runtime cannot
    // balance the stack after the call, and a game that calls a few of them
    // during start-up returns into rubbish.
    {"d3d9.dll", "Direct3DCreate9", 1, nullptr},
    {"d3dx9_26.dll", "D3DXCreateEffectPool", 1, nullptr},
    {"d3dx9_26.dll", "D3DXCreateEffectFromResourceA", 9, nullptr},
    {"d3dx9_26.dll", "D3DXMatrixMultiply", 3, nullptr},
    {"d3dx9_26.dll", "D3DXMatrixInverse", 3, nullptr},
    {"d3dx9_26.dll", "D3DXMatrixTranspose", 2, nullptr},
    {"d3dx9_26.dll", "D3DXMatrixOrthoLH", 5, nullptr},
    {"d3dx9_26.dll", "D3DXMatrixPerspectiveLH", 5, nullptr},
    {"d3dx9_26.dll", "D3DXMatrixTranslation", 4, nullptr},
    {"d3dx9_26.dll", "D3DXVec4Transform", 3, nullptr},
    {"d3dx9_26.dll", "D3DXVec3Transform", 3, nullptr},
    {"d3dx9_26.dll", "D3DXVec3TransformNormal", 3, nullptr},
    {"d3dx9_26.dll", "D3DXVec3TransformCoordArray", 6, nullptr},
    {"d3dx9_26.dll", "D3DXVec3Normalize", 2, nullptr},
    {"DINPUT8.dll", "DirectInput8Create", 5, nullptr},
    {"DSOUND.dll", "ord6", 3, nullptr}, // DirectSoundCaptureCreate
    {"WINMM.dll", "waveOutGetNumDevs", 0, m_waveGetNumDevs},
    {"WINMM.dll", "waveOutGetDevCapsA", 3, m_waveBadDevice},
    {"WINMM.dll", "waveOutOpen", 6, m_waveBadDevice},
    {"WINMM.dll", "waveOutClose", 1, m_waveInvalidHandle},
    {"WINMM.dll", "waveOutPrepareHeader", 3, m_waveInvalidHandle},
    {"WINMM.dll", "waveOutUnprepareHeader", 3, m_waveInvalidHandle},
    {"WINMM.dll", "waveOutWrite", 3, m_waveInvalidHandle},
    {"WINMM.dll", "waveOutReset", 1, m_waveInvalidHandle},
    {"WINMM.dll", "waveOutGetPosition", 3, m_waveInvalidHandle},
    {"WINMM.dll", "waveOutSetVolume", 2, m_waveInvalidHandle},
    {"WINMM.dll", "waveInGetNumDevs", 0, m_waveGetNumDevs},
    {"WINMM.dll", "waveInGetDevCapsA", 3, m_waveBadDevice},
    {"WINMM.dll", "waveInOpen", 6, m_waveBadDevice},
    {"WINMM.dll", "waveInClose", 1, m_waveInvalidHandle},
    {"WINMM.dll", "waveInPrepareHeader", 3, m_waveInvalidHandle},
    {"WINMM.dll", "waveInUnprepareHeader", 3, m_waveInvalidHandle},
    {"WINMM.dll", "waveInAddBuffer", 3, m_waveInvalidHandle},
    {"WINMM.dll", "waveInStart", 1, m_waveInvalidHandle},
    {"WINMM.dll", "waveInStop", 1, m_waveInvalidHandle},
    {"WINMM.dll", "waveInReset", 1, m_waveInvalidHandle},
    {"WINMM.dll", "waveInGetPosition", 3, m_waveInvalidHandle},
    // Winsock 2, offline. LHMultiplayerR delay-loads this DLL by ordinal, so
    // its ImgDelayDescr names ordinals rather than names; the table below
    // covers exactly that ordinal set (plus the byte-order twins). WSAStartup
    // and WSACleanup share the WSOCK32 implementation. Every call that would
    // open or use a socket returns the real offline failure; name lookups
    // resolve localhost only; the byte-order and address utilities are exact.
    // See the offline-Winsock block above; networking is a deferred gap.
    {"WS2_32.dll", "WSAStartup", 2, w_WSAStartup},
    {"WS2_32.dll", "ord115", 2, w_WSAStartup},
    {"WS2_32.dll", "WSACleanup", 0, w_WSACleanup},
    {"WS2_32.dll", "ord116", 0, w_WSACleanup},
    {"WS2_32.dll", "WSAGetLastError", 0, w_WSAGetLastError},
    {"WS2_32.dll", "ord111", 0, w_WSAGetLastError},
    {"WS2_32.dll", "WSASetLastError", 1, w_WSASetLastError},
    {"WS2_32.dll", "ord112", 1, w_WSASetLastError},
    {"WS2_32.dll", "accept", 3, w_accept},
    {"WS2_32.dll", "ord1", 3, w_accept},
    {"WS2_32.dll", "bind", 3, w_bind},
    {"WS2_32.dll", "ord2", 3, w_bind},
    {"WS2_32.dll", "closesocket", 1, w_closesocket},
    {"WS2_32.dll", "ord3", 1, w_closesocket},
    {"WS2_32.dll", "connect", 3, w_connect},
    {"WS2_32.dll", "ord4", 3, w_connect},
    {"WS2_32.dll", "getpeername", 3, w_getpeername},
    {"WS2_32.dll", "ord5", 3, w_getpeername},
    {"WS2_32.dll", "getsockname", 3, w_getsockname},
    {"WS2_32.dll", "ord6", 3, w_getsockname},
    {"WS2_32.dll", "getsockopt", 5, w_getsockopt},
    {"WS2_32.dll", "ord7", 5, w_getsockopt},
    {"WS2_32.dll", "htonl", 1, w_htonl},
    {"WS2_32.dll", "ord8", 1, w_htonl},
    {"WS2_32.dll", "htons", 1, w_htons},
    {"WS2_32.dll", "ord9", 1, w_htons},
    {"WS2_32.dll", "inet_addr", 1, w_inet_addr},
    {"WS2_32.dll", "ord10", 1, w_inet_addr},
    {"WS2_32.dll", "inet_ntoa", 1, w_inet_ntoa},
    {"WS2_32.dll", "ord11", 1, w_inet_ntoa},
    {"WS2_32.dll", "ioctlsocket", 3, w_ioctlsocket},
    {"WS2_32.dll", "ord12", 3, w_ioctlsocket},
    {"WS2_32.dll", "listen", 2, w_listen},
    {"WS2_32.dll", "ord13", 2, w_listen},
    {"WS2_32.dll", "ntohl", 1, w_ntohl},
    {"WS2_32.dll", "ord14", 1, w_ntohl},
    {"WS2_32.dll", "ntohs", 1, w_ntohs},
    {"WS2_32.dll", "ord15", 1, w_ntohs},
    {"WS2_32.dll", "recv", 4, w_recv},
    {"WS2_32.dll", "ord16", 4, w_recv},
    {"WS2_32.dll", "recvfrom", 6, w_recvfrom},
    {"WS2_32.dll", "ord17", 6, w_recvfrom},
    {"WS2_32.dll", "select", 5, w_select},
    {"WS2_32.dll", "ord18", 5, w_select},
    {"WS2_32.dll", "send", 4, w_send},
    {"WS2_32.dll", "ord19", 4, w_send},
    {"WS2_32.dll", "sendto", 6, w_sendto},
    {"WS2_32.dll", "ord20", 6, w_sendto},
    {"WS2_32.dll", "setsockopt", 5, w_setsockopt},
    {"WS2_32.dll", "ord21", 5, w_setsockopt},
    {"WS2_32.dll", "shutdown", 2, w_shutdown},
    {"WS2_32.dll", "ord22", 2, w_shutdown},
    {"WS2_32.dll", "socket", 3, w_socket},
    {"WS2_32.dll", "ord23", 3, w_socket},
    {"WS2_32.dll", "gethostbyaddr", 3, w_gethostbyaddr},
    {"WS2_32.dll", "ord51", 3, w_gethostbyaddr},
    {"WS2_32.dll", "gethostbyname", 1, w_gethostbyname},
    {"WS2_32.dll", "ord52", 1, w_gethostbyname},
    {"WS2_32.dll", "gethostname", 2, w_gethostname},
    {"WS2_32.dll", "ord57", 2, w_gethostname},
    {"WS2_32.dll", "__WSAFDIsSet", 2, w_wsa_fd_is_set},
    {"WS2_32.dll", "ord151", 2, w_wsa_fd_is_set},
    {"WS2_32.dll", "WSAIoctl", 9, w_WSAIoctl},
    {"WS2_32.dll", "WSACreateEvent", 0, w_WSACreateEvent},
    {"WS2_32.dll", "WSACloseEvent", 1, w_WSACloseEvent},
    {"WS2_32.dll", "WSASetEvent", 1, w_WSASetEvent},
    {"WS2_32.dll", "WSAResetEvent", 1, w_WSAResetEvent},
    {"WS2_32.dll", "WSAWaitForMultipleEvents", 5, w_WSAWaitForMultipleEvents},
    {"WS2_32.dll", "WSARecv", 7, w_WSARecv},
    {"WS2_32.dll", "WSARecvFrom", 9, w_WSARecvFrom},
    {"WS2_32.dll", "WSAGetOverlappedResult", 5, w_WSAGetOverlappedResult},
    {"NETAPI32.dll", "Netbios", 1, nullptr},
    {"DDRAW.dll", "DirectDrawCreate", 3, nullptr},
    {"DDRAW.dll", "DirectDrawEnumerateA", 2, nullptr},
    {"DINPUT.dll", "DirectInputCreateA", 4, nullptr},
    {"DSOUND.dll", "ord1", 3, nullptr}, // DirectSoundCreate

    // QMIXER and weanetr: the audio and network tasks own the behaviour, but a
    // callee-cleanup DLL must pop its arguments or the guest stack drifts. The
    // counts below are recovered, not guessed: each one is the `ret n` the
    // export actually executes in original/gog/QMixer.dll and
    // original/gog/WEANETR.dll, reached by a fall-through walk from the export
    // address. For weanetr every count independently matches the MSVC name
    // mangling (`QAE` = __thiscall, `this` in ECX, the rest on the stack), so
    // the two derivations agree on all 17 methods.
    {"QMIXER.dll", "QSWaveMixSetSpeakerPlacement", 2, nullptr},
    {"QMIXER.dll", "QSWaveMixSetSpeedOfSound", 3, nullptr},
    {"QMIXER.dll", "QSWaveMixSetPanRate", 4, nullptr},
    {"QMIXER.dll", "QSWaveMixSetListenerOrientation", 4, nullptr},
    {"QMIXER.dll", "QSWaveMixSetListenerPosition", 3, nullptr},
    {"QMIXER.dll", "QSWaveMixSetVolume", 4, nullptr},
    {"QMIXER.dll", "QSWaveMixSetDistanceMapping", 4, nullptr},
    {"QMIXER.dll", "QSWaveMixSetSourceCone", 6, nullptr},
    {"QMIXER.dll", "QSWaveMixSetFrequency", 4, nullptr},
    {"QMIXER.dll", "QSWaveMixSetListenerVelocity", 3, nullptr},
    {"QMIXER.dll", "QSWaveMixSetSourceVelocity", 4, nullptr},
    {"QMIXER.dll", "QSWaveMixSetSourcePosition", 4, nullptr},
    {"QMIXER.dll", "QSWaveMixSetPosition", 4, nullptr},
    {"QMIXER.dll", "QSWaveMixRestartChannel", 3, nullptr},
    {"QMIXER.dll", "QSWaveMixPauseChannel", 3, nullptr},
    {"QMIXER.dll", "QSWaveMixStopChannel", 3, nullptr},
    {"QMIXER.dll", "QSWaveMixConfigureChannel", 5, nullptr},
    {"QMIXER.dll", "QSWaveMixGetChannelParams", 3, nullptr},
    {"QMIXER.dll", "QSWaveMixEnableChannel", 4, nullptr},
    {"QMIXER.dll", "QSWaveMixOpenWaveEx", 3, nullptr},
    {"QMIXER.dll", "QSWaveMixFreeWave", 2, nullptr},
    {"QMIXER.dll", "QSWaveMixPlayEx", 6, nullptr},
    {"AVIFIL32.dll", "AVIFileInit", 0, avi_noop},
    {"AVIFIL32.dll", "AVIFileExit", 0, avi_noop},
    {"AVIFIL32.dll", "AVIFileOpenA", 4, avi_open_unsupported},
    {"AVIFIL32.dll", "AVIFileGetStream", 4, avi_bad_handle},
    {"AVIFIL32.dll", "AVIFileRelease", 1, avi_noop},
    {"AVIFIL32.dll", "AVIStreamRelease", 1, avi_noop},
    {"AVIFIL32.dll", "AVIStreamInfoA", 3, avi_bad_handle},
    {"AVIFIL32.dll", "AVIStreamReadFormat", 4, avi_bad_handle},
    {"AVIFIL32.dll", "AVIStreamRead", 7, avi_bad_handle},
    {"AVIFIL32.dll", "AVIStreamBeginStreaming", 4, avi_bad_handle},
    {"AVIFIL32.dll", "AVIStreamEndStreaming", 1, avi_bad_handle},
    {"AVIFIL32.dll", "AVIStreamFindSample", 3, avi_no_sample},
    {"AVIFIL32.dll", "AVIStreamSampleToTime", 2, avi_no_sample},
    {"AVIFIL32.dll", "AVIStreamTimeToSample", 2, avi_no_sample},
    {"QMIXER.dll", "QSWaveMixCloseSession", 1, nullptr},
    {"QMIXER.dll", "QSWaveMixGetDirectSound", 2, nullptr},
    {"QMIXER.dll", "QSWaveMixInitEx", 1, nullptr},
    {"QMIXER.dll", "QSWaveMixActivate", 2, nullptr},
    {"QMIXER.dll", "QSWaveMixOpenChannel", 3, nullptr},
    {"QMIXER.dll", "QSWaveMixSetOptions", 3, nullptr},
    {"QMIXER.dll", "QSWaveMixPump", 0, nullptr},
    {"weanetr.dll", "?SendData@MLDPlay@@QAEHKPAXKKPAK@Z", 5, nullptr},
    {"weanetr.dll", "?GetPlayerInfo@MLDPlay@@QAEHPAUMLDPLAY_PLAYERINFO@@@Z", 1, nullptr},
    {"weanetr.dll", "?EnumerateServices@MLDPlay@@QAEHP6GXPAXPAGPAU_GUID@@K0@Z0@Z", 2, nullptr},
    {"weanetr.dll",
     "?AreWeLobbied@MLDPlay@@QAEKP6GXKPAXKK0@ZPAU_GUID@@PAUMLDPLAY_LOBBYINFO@@PAKPAG5KK@Z", 8,
     nullptr},
    {"weanetr.dll", "?StartupNetwork@MLDPlay@@QAEHP6GXKPAXKK0@Z@Z", 1, nullptr},
    {"weanetr.dll", "?ShutdownNetwork@MLDPlay@@QAEHXZ", 0, nullptr},
    {"weanetr.dll", "?SendMSResults@MLDPlay@@QAEKPAD@Z", 1, nullptr},
    {"weanetr.dll", "?EnumerateNetworkMediums@MLDPlay@@QAEKP6GXPAGPAX@Z1@Z", 2, nullptr},
    {"weanetr.dll", "?GetCurrentMs@MLDPlay@@QAEKXZ", 0, nullptr},
    {"weanetr.dll", "?CreateSession@MLDPlay@@QAEHPAKPAG1PAXK@Z", 5, nullptr},
    {"weanetr.dll", "?EnumerateSessions@MLDPlay@@QAEHKP6GXPAUMLDPLAY_SESSIONDESC@@PAX@ZK1@Z", 4,
     nullptr},
    {"weanetr.dll", "?JoinSession@MLDPlay@@QAEHPAUMLDPLAY_SESSIONDESC@@PAKPAGPAX@Z", 4, nullptr},
    {"weanetr.dll", "?CreateNetworkAddress@MLDPlay@@QAEHPAXK0PAK@Z", 4, nullptr},
    {"weanetr.dll", "?EnableNewPlayers@MLDPlay@@QAEXH@Z", 1, nullptr},
    {"weanetr.dll", "?DestroySession@MLDPlay@@QAEHXZ", 0, nullptr},
    {"weanetr.dll", "?SetupConnection@MLDPlay@@QAEHPAXPAU_GUID@@0@Z", 3, nullptr},
    {"weanetr.dll", "?SendChat@MLDPlay@@QAEHKPAGKPAK@Z", 4, nullptr},
};
const size_t g_misc_shim_count = sizeof(g_misc_shims) / sizeof(g_misc_shims[0]);
