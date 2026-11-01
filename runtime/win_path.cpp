#include "win_path.h"
#include "win32.h"
#include "native_seam.h"
#include "game_config.h"
#include <algorithm>
#include <cctype>
#include <cstring>

static_assert((int)WIN32_FILE_READ == (int)VFS_READ && (int)WIN32_FILE_WRITE == (int)VFS_WRITE &&
              (int)WIN32_FILE_DELETE == (int)VFS_DELETE &&
              (int)WIN32_FILE_RENAME_SRC == (int)VFS_RENAME_SRC &&
              (int)WIN32_FILE_RENAME_DST == (int)VFS_RENAME_DST &&
              (int)WIN32_FILE_LIST == (int)VFS_LIST);

namespace {
std::string g_cur_dir = RECOMP_GUEST_ROOT;
bool g_initialized = false;
void ensure_initialized() {
    if (!g_initialized) {
        std::string exe = RECOMP_DEVELOPER_EXE;
        vfs_init(exe.substr(0, exe.rfind('/')), true);
        g_initialized = true;
    }
}
std::string lower(std::string s) {
    for (char &ch : s)
        ch = (char)tolower((unsigned char)ch);
    return s;
}
std::vector<std::string> split_path(const std::string &p) {
    std::vector<std::string> out;
    std::string cur;
    for (char ch : p) {
        if (ch == '/' || ch == '\\') {
            if (!cur.empty())
                out.push_back(cur);
            cur.clear();
        } else {
            cur.push_back(ch);
        }
    }
    if (!cur.empty())
        out.push_back(cur);
    return out;
}

} // namespace

void win_path_init(const std::string &game_dir) {
    vfs_init(game_dir.empty() ? "." : game_dir, true);
    g_initialized = true;
    g_cur_dir = RECOMP_GUEST_ROOT;
}
const std::string &win_path_current() {
    return g_cur_dir;
}
void win_path_set_current(const std::string &path) {
    g_cur_dir = path;
}
VfsPath win_path_components(const std::string &guest_path) {
    ensure_initialized();
    std::string p = guest_path;
    bool absolute = false;
    if (p.size() >= 2 && p[1] == ':') {
        p = p.substr(2);
        absolute = true;
    }
    if (!p.empty() && (p[0] == '\\' || p[0] == '/'))
        absolute = true;

    std::vector<std::string> comps;
    if (!absolute) {
        std::string base = g_cur_dir;
        if (base.size() >= 2 && base[1] == ':')
            base = base.substr(2);
        comps = split_path(base);
    }
    for (const std::string &c : split_path(p))
        comps.push_back(c);

    std::vector<std::string> norm;
    for (const std::string &c : comps) {
        if (c == ".")
            continue;
        if (c == "..") {
            if (!norm.empty())
                norm.pop_back();
            continue;
        }
        norm.push_back(c);
    }
    static const std::vector<std::string> root = [] {
        std::string r = RECOMP_GUEST_ROOT;
        if (r.size() >= 2 && r[1] == ':')
            r = r.substr(2);
        return split_path(r);
    }();
    size_t common = 0;
    while (common < root.size() && common < norm.size() &&
           lower(norm[common]) == lower(root[common]))
        ++common;
    if (!root.empty() && common == root.size()) {
        norm.erase(norm.begin(), norm.begin() + (long)root.size());
    } else if (common > 0) {
        norm.erase(norm.begin(), norm.begin() + (long)common);
        norm.insert(norm.begin(), root.size() - common, std::string(".."));
    } else if (!norm.empty() && lower(norm[0]) == lower(win32_guest_root_name())) {
        norm.erase(norm.begin());
    }
    return norm;
}

std::string win32_host_path_op(const std::string &guest_path, int op) {
    return guest_path.empty() ? std::string() : vfs_resolve(win_path_components(guest_path), op);
}
std::string win32_host_path(const std::string &guest_path, bool create) {
    return win32_host_path_op(guest_path, create ? WIN32_FILE_WRITE : WIN32_FILE_READ);
}
std::string win32_guest_path(const std::string &host_path) {
    ensure_initialized();
    std::string rel = host_path;
    const std::string &root = vfs_root();
    if (rel.compare(0, root.size(), root) == 0)
        rel = rel.substr(root.size());
    std::string out = RECOMP_GUEST_ROOT;
    for (const std::string &component : split_path(rel))
        out += "\\" + component;
    return out;
}
const std::string &win32_game_dir() {
    ensure_initialized();
    return vfs_root();
}
void win32_set_file_ops(VfsResolve resolve, VfsList list) {
    vfs_set_file_ops(resolve, list);
}
void win32_set_dir_cache_hook(void (*hook)()) {
    vfs_set_cache_hook(hook);
}
void win32_invalidate_dir_cache() {
    vfs_invalidate_cache();
}
extern "C" int recomp_writable_path(const char *guest, char *out, size_t len) {
    return guest && out && len && vfs_resolve_writable(win_path_components(guest), out, len);
}
extern "C" int recomp_readable_path(const char *guest, char *out, size_t len) {
    if (!guest || !out || !len)
        return 0;
    std::string path = win32_host_path_op(guest, WIN32_FILE_READ);
    if (path.empty() || path.size() >= len)
        return 0;
    memcpy(out, path.c_str(), path.size() + 1);
    return 1;
}
extern "C" int recomp_file_alias_add(const char *guest, const char *host) {
    return guest && *guest && host && *host && vfs_alias_add(win_path_components(guest), host);
}
extern "C" void recomp_file_alias_remove(const char *guest) {
    if (guest && *guest)
        vfs_alias_remove(win_path_components(guest));
}
std::string full_path_named(const std::string &name) {
    std::string full;
    if (name.size() >= 2 && name[1] == ':')
        full = name;
    else if (!name.empty() && (name[0] == '\\' || name[0] == '/'))
        full = "C:" + name;
    else
        full = g_cur_dir + "\\" + name;
    std::replace(full.begin(), full.end(), '/', '\\');
    if (full.size() >= 3 && full[1] == ':' && full[2] == '\\') {
        std::vector<std::string> components;
        for (const auto &component : split_path(full.substr(3))) {
            if (component == ".")
                continue;
            if (component == "..") {
                if (!components.empty())
                    components.pop_back();
            } else {
                components.push_back(component);
            }
        }
        full.resize(3);
        for (const auto &component : components) {
            if (full.size() > 3)
                full += '\\';
            full += component;
        }
    }
    return full;
}
