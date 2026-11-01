#include "vfs.h"
#include "../platform/os.h"
#include <atomic>
#include <cctype>
#include <mutex>
#include <map>

namespace {
std::string g_root = ".";
bool g_case_insensitive = true;
VfsResolve g_resolve = nullptr;
VfsList g_list = nullptr;
void (*g_cache_hook)() = nullptr;
std::map<std::string, std::map<std::string, std::string>> g_cache;
std::mutex g_alias_lock;
std::atomic<bool> g_alias_live{false};

std::map<std::string, std::string> &aliases() {
    static std::map<std::string, std::string> entries;
    return entries;
}
std::string key(std::string value) {
    if (g_case_insensitive)
        for (char &ch : value)
            ch = (char)tolower((unsigned char)ch);
    return value;
}
std::string relative(const VfsPath &path) {
    std::string result;
    for (size_t i = 0; i < path.size(); ++i) {
        if (i)
            result += '/';
        result += path[i];
    }
    return result;
}
int collect(const char *name, void *ctx) {
    auto *entries = static_cast<std::map<std::string, std::string> *>(ctx);
    entries->emplace(key(name), name);
    return 0;
}
const std::map<std::string, std::string> &listing(const std::string &dir) {
    auto it = g_cache.find(dir);
    if (it != g_cache.end())
        return it->second;
    std::map<std::string, std::string> entries;
    os_listdir(dir.c_str(), collect, &entries);
    return g_cache.emplace(dir, std::move(entries)).first->second;
}
std::string resolve_root(const VfsPath &path, bool create) {
    if (path.size() > 1) {
        std::string direct = g_root + "/" + relative(path);
        OsStat st;
        if (os_stat(direct.c_str(), &st) == 0)
            return direct;
    }
    std::string host = g_root;
    for (size_t i = 0; i < path.size(); ++i) {
        std::string next = host + "/" + path[i];
        OsStat st;
        if (os_stat(next.c_str(), &st) == 0) {
            host = next;
            continue;
        }
        if (g_case_insensitive) {
            const auto &entries = listing(host);
            auto it = entries.find(key(path[i]));
            if (it != entries.end()) {
                host += "/" + it->second;
                continue;
            }
        }
        if (create && i + 1 == path.size())
            return next;
        return {};
    }
    return host;
}
} // namespace

void vfs_init(const std::string &root, bool case_insensitive) {
    g_root = root;
    g_case_insensitive = case_insensitive;
    g_cache.clear();
}
const std::string &vfs_root() {
    return g_root;
}
void vfs_set_file_ops(VfsResolve resolve, VfsList list) {
    g_resolve = resolve;
    g_list = list;
    vfs_invalidate_cache();
}
void vfs_set_cache_hook(void (*hook)()) {
    g_cache_hook = hook;
}
void vfs_invalidate_cache() {
    g_cache.clear();
    if (g_cache_hook)
        g_cache_hook();
}
std::string vfs_resolve(const VfsPath &path, int op) {
    std::string rel = relative(path);
    if (op == VFS_READ && g_alias_live.load(std::memory_order_acquire)) {
        std::lock_guard<std::mutex> lock(g_alias_lock);
        auto it = aliases().find(key(rel));
        if (it != aliases().end())
            return it->second;
    }
    if (g_resolve) {
        char out[1024];
        if (g_resolve(rel.c_str(), op, out, sizeof out))
            return out;
        if (op != VFS_READ && op != VFS_LIST)
            return {};
    }
    return resolve_root(path, op == VFS_WRITE || op == VFS_RENAME_DST);
}
bool vfs_resolve_writable(const VfsPath &path, char *out, size_t size) {
    return g_resolve && g_resolve(relative(path).c_str(), VFS_WRITE, out, size);
}
bool vfs_alias_add(const VfsPath &path, const std::string &host) {
    std::string rel = key(relative(path));
    if (rel.empty())
        return false;
    std::lock_guard<std::mutex> lock(g_alias_lock);
    aliases()[rel] = host;
    g_alias_live.store(true, std::memory_order_release);
    return true;
}
void vfs_alias_remove(const VfsPath &path) {
    std::lock_guard<std::mutex> lock(g_alias_lock);
    aliases().erase(key(relative(path)));
    g_alias_live.store(!aliases().empty(), std::memory_order_release);
}
bool vfs_has_lister() {
    return g_list != nullptr;
}
void vfs_list(const VfsPath &path, const std::string &host_dir, VfsEmit emit, void *ctx) {
    if (g_list) {
        std::string rel = relative(path);
        g_list(rel.c_str(), emit, ctx);
        return;
    }
    for (const auto &entry : listing(host_dir)) {
        std::string host = host_dir + "/" + entry.second;
        emit(ctx, entry.second.c_str(), host.c_str());
    }
}
bool vfs_promote(const VfsPath &path, int &fd, std::string &host, bool &pending, int flags) {
    if (!pending)
        return true;
    std::string target = vfs_resolve(path, VFS_WRITE);
    if (target.empty())
        return false;
    int64_t pos = os_fd_seek(fd, 0, OS_SEEK_CUR);
    int next = os_fd_open(target.c_str(), flags | OS_O_CREAT);
    if (next < 0)
        return false;
    if (pos > 0)
        os_fd_seek(next, pos, OS_SEEK_SET);
    os_fd_close(fd);
    fd = next;
    host = target;
    pending = false;
    vfs_invalidate_cache();
    return true;
}
