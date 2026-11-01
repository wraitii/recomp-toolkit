#pragma once
#include <cstddef>
#include <string>
#include <vector>

using VfsPath = std::vector<std::string>;
using VfsEmit = void (*)(void *, const char *, const char *);
using VfsResolve = int (*)(const char *, int, char *, size_t);
using VfsList = void (*)(const char *, VfsEmit, void *);

enum VfsOp { VFS_READ, VFS_WRITE, VFS_DELETE, VFS_RENAME_SRC, VFS_RENAME_DST, VFS_LIST };

void vfs_init(const std::string &root, bool case_insensitive);
const std::string &vfs_root();
void vfs_set_file_ops(VfsResolve resolve, VfsList list);
void vfs_set_cache_hook(void (*hook)());
void vfs_invalidate_cache();
std::string vfs_resolve(const VfsPath &path, int op);
bool vfs_resolve_writable(const VfsPath &path, char *out, size_t size);
bool vfs_alias_add(const VfsPath &path, const std::string &host);
void vfs_alias_remove(const VfsPath &path);
bool vfs_has_lister();
void vfs_list(const VfsPath &path, const std::string &host_dir, VfsEmit emit, void *ctx);
bool vfs_promote(const VfsPath &path, int &fd, std::string &host, bool &pending, int flags);
