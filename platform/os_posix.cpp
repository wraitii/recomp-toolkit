// os_posix.cpp - the platform layer on macOS and Linux. The only file above
// third_party/ that may include a POSIX or Mach header besides os_win32.cpp.
#include "os.h"

#include <string>
#include <vector>

#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <spawn.h>
#include <sys/wait.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <sys/mman.h>
#include <sys/stat.h>
#ifdef __APPLE__
#include <TargetConditionals.h>
#if TARGET_OS_OSX
#include <libproc.h>
#endif
#endif
#include <sys/file.h>
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/uio.h>
#include <sys/socket.h>
#include <sys/statvfs.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>
#ifdef __linux__
#include <sys/syscall.h>
#endif
#if !defined(__ANDROID__)
#include <execinfo.h>
#endif
#ifdef __APPLE__
#include <mach-o/dyld.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <CoreFoundation/CoreFoundation.h>
#include <TargetConditionals.h>
#if TARGET_OS_OSX
#include <CoreServices/CoreServices.h>
#include <dispatch/dispatch.h>
#include <atomic>
#endif
#endif
#ifndef MAP_ANON
#define MAP_ANON MAP_ANONYMOUS
#endif

struct OsThread {
    pthread_t handle;
};

#if defined(__APPLE__) && TARGET_OS_OSX
struct OsDirectoryWatch {
    FSEventStreamRef stream = nullptr;
    dispatch_queue_t queue = nullptr;
    std::string path;
    bool subtree = false;
    std::atomic<bool> pending{false};
    std::atomic<bool> overflow{false};
};

static void directory_watch_events(ConstFSEventStreamRef, void *context, size_t count, void *paths,
                                   const FSEventStreamEventFlags flags[],
                                   const FSEventStreamEventId[]) {
    auto *watch = static_cast<OsDirectoryWatch *>(context);
    auto **names = static_cast<char **>(paths);
    for (size_t i = 0; i < count; ++i) {
        if (flags[i] &
            (kFSEventStreamEventFlagMustScanSubDirs | kFSEventStreamEventFlagUserDropped |
             kFSEventStreamEventFlagKernelDropped)) {
            watch->overflow.store(true);
            continue;
        }
        if (!(flags[i] & kFSEventStreamEventFlagItemModified))
            continue;
        std::string name = names[i];
        if (name.compare(0, watch->path.size(), watch->path) != 0)
            continue;
        std::string relative = name.substr(watch->path.size());
        if (relative.empty() || (!watch->subtree && relative.find('/') != std::string::npos))
            continue;
        watch->pending.store(true);
    }
}
#endif

extern "C" {

const char *os_platform_name(void) {
#ifdef __APPLE__
    return "macos";
#else
    return "linux";
#endif
}

OsThread *os_thread_create(void *(*fn)(void *), void *arg, size_t stack_bytes) {
    OsThread *t = (OsThread *)calloc(1, sizeof *t);
    if (!t)
        return nullptr;
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    if (stack_bytes)
        pthread_attr_setstacksize(&attr, stack_bytes);
    int rc = pthread_create(&t->handle, &attr, fn, arg);
    pthread_attr_destroy(&attr);
    if (rc != 0) {
        free(t);
        return nullptr;
    }
    return t;
}
void os_thread_join(OsThread *t) {
    pthread_join(t->handle, nullptr);
    free(t);
}
void os_thread_detach(OsThread *t) {
    pthread_detach(t->handle);
    free(t);
}
void os_thread_prefer_performance(void) {
#ifdef __APPLE__
    pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
#endif
}
void os_thread_exit(void) {
    pthread_exit(nullptr);
}
OsThreadId os_thread_self(void) {
    return (OsThreadId)(uintptr_t)pthread_self();
}
uint32_t os_profile_thread_id(void) {
#ifdef __APPLE__
    uint64_t id = 0;
    pthread_threadid_np(nullptr, &id);
    return (uint32_t)id;
#else
    return (uint32_t)syscall(SYS_gettid);
#endif
}
OsThreadId os_thread_id_of(const OsThread *t) {
    return (OsThreadId)(uintptr_t)t->handle;
}

void *os_vm_reserve(size_t bytes) {
    void *p = mmap(nullptr, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    return p == MAP_FAILED ? nullptr : p;
}
void *os_vm_reserve_at(void *address, size_t bytes) {
#ifdef __APPLE__
    mach_vm_address_t base = (mach_vm_address_t)(uintptr_t)address;
    kern_return_t result = mach_vm_allocate(mach_task_self(), &base, bytes, VM_FLAGS_FIXED);
    if (result != KERN_SUCCESS) {
        fprintf(stderr, "[platform] fixed VM allocation at %p (%zu bytes): %s (%d)\n", address,
                bytes, mach_error_string(result), result);
        return nullptr;
    }
    return (void *)(uintptr_t)base;
#else
    // A hint without MAP_FIXED never replaces a mapping; the kernel places the
    // region elsewhere when the range is taken, which is then a failure here.
    void *p = mmap(address, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    if (p == MAP_FAILED)
        return nullptr;
    if (p != address) {
        munmap(p, bytes);
        return nullptr;
    }
    return p;
#endif
}
void os_vm_release(void *p, size_t bytes) {
    munmap(p, bytes);
}

void *os_dlopen(const char *path) {
    return dlopen(path, RTLD_NOW | RTLD_LOCAL);
}
void *os_dlopen_noload(const char *path) {
    return dlopen(path, RTLD_NOLOAD | RTLD_LOCAL);
}
void *os_dlsym(void *handle, const char *name) {
    return dlsym(handle, name);
}
int os_dlclose(void *handle) {
    return dlclose(handle);
}
const char *os_dlerror(void) {
    const char *e = dlerror();
    return e ? e : "unknown dynamic loader error";
}
const char *os_plugin_extension(void) {
#ifdef __APPLE__
    return ".dylib";
#else
    return ".so";
#endif
}

static void fill_stat(const struct stat &st, OsStat *out) {
    out->size = (uint64_t)st.st_size;
    out->atime = (int64_t)st.st_atime;
    out->mtime = (int64_t)st.st_mtime;
    out->ctime = (int64_t)st.st_ctime;
    out->ino = (uint64_t)st.st_ino;
    out->is_dir = S_ISDIR(st.st_mode) ? 1 : 0;
    out->is_regular = S_ISREG(st.st_mode) ? 1 : 0;
    out->is_symlink = S_ISLNK(st.st_mode) ? 1 : 0;
    out->is_readonly = (st.st_mode & S_IWUSR) ? 0 : 1;
}
int os_stat(const char *path, OsStat *out) {
    struct stat st;
    if (stat(path, &st) != 0)
        return -1;
    fill_stat(st, out);
    return 0;
}
int os_lstat(const char *path, OsStat *out) {
    struct stat st;
    if (lstat(path, &st) != 0)
        return -1;
    fill_stat(st, out);
    return 0;
}
int os_mkdir(const char *path) {
    return mkdir(path, 0755);
}
int os_rename(const char *from, const char *to) {
    return rename(from, to);
}
int os_unlink(const char *path) {
    return unlink(path);
}
int os_rmdir(const char *path) {
    return rmdir(path);
}
int os_getcwd(char *buf, size_t cap) {
    return getcwd(buf, cap) ? 0 : -1;
}
int os_realpath(const char *path, char *buf, size_t cap) {
    char *resolved = realpath(path, nullptr);
    if (!resolved)
        return -1;
    const size_t size = strlen(resolved) + 1;
    if (size > cap) {
        free(resolved);
        errno = ERANGE;
        return -1;
    }
    memcpy(buf, resolved, size);
    free(resolved);
    return 0;
}

int os_directory_contains(const char *path, const char *name) {
    DIR *directory = opendir(path);
    if (!directory)
        return 0;
    const size_t wanted = strlen(name);
    struct dirent *entry;
    int found = 0;
    while ((entry = readdir(directory)))
        if (strlen(entry->d_name) == wanted && strcasecmp(entry->d_name, name) == 0) {
            struct stat info{};
            std::string child = std::string(path) + "/" + entry->d_name;
            found = stat(child.c_str(), &info) == 0 && S_ISDIR(info.st_mode);
            break;
        }
    closedir(directory);
    return found;
}
int os_chdir(const char *path) {
    return chdir(path);
}
int os_listdir(const char *dir, OsListDirFn fn, void *user) {
    DIR *d = opendir(dir);
    if (!d)
        return -1;
    while (struct dirent *e = readdir(d)) {
        if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0)
            continue;
        if (fn(e->d_name, user) != 0)
            break;
    }
    closedir(d);
    return 0;
}
int os_mkstemp(char *template_path) {
    return mkstemp(template_path);
}

struct OsFileLock {
    int fd;
};

OsFileLock *os_file_lock(const char *path) {
    int flags = O_CREAT | O_RDWR | O_CLOEXEC;
#ifdef O_NOFOLLOW
    flags |= O_NOFOLLOW;
#endif
    int fd = open(path, flags, 0600);
    if (fd < 0)
        return nullptr;
    struct stat file_info;
    int stat_result = fstat(fd, &file_info);
    if (stat_result != 0 || !S_ISREG(file_info.st_mode)) {
        const int error = stat_result != 0 ? errno : EINVAL;
        close(fd);
        errno = error;
        return nullptr;
    }
    if (flock(fd, LOCK_EX | LOCK_NB) != 0) {
        const int error = errno;
        close(fd);
        errno = error;
        return nullptr;
    }
    double start_time = 0;
#if defined(__APPLE__) && TARGET_OS_OSX
    struct proc_bsdinfo info{};
    if (proc_pidinfo(getpid(), PROC_PIDTBSDINFO, 0, &info, sizeof info) == sizeof info)
        start_time = (double)info.pbi_start_tvsec + (double)info.pbi_start_tvusec / 1000000.0;
#elif defined(__linux__)
    FILE *stat_file = fopen("/proc/self/stat", "r");
    FILE *system_file = fopen("/proc/stat", "r");
    long long ticks = 0, boot = 0;
    if (stat_file && system_file) {
        char line[4096];
        if (fgets(line, sizeof line, stat_file)) {
            char *tail = strrchr(line, ')');
            if (tail) {
                char *save = nullptr;
                char *field = strtok_r(tail + 2, " ", &save);
                for (int index = 0; field && index < 19; ++index)
                    field = strtok_r(nullptr, " ", &save);
                if (field)
                    ticks = strtoll(field, nullptr, 10);
            }
        }
        while (fgets(line, sizeof line, system_file))
            if (sscanf(line, "btime %lld", &boot) == 1)
                break;
    }
    if (stat_file)
        fclose(stat_file);
    if (system_file)
        fclose(system_file);
    long hz = sysconf(_SC_CLK_TCK);
    if (ticks && boot && hz > 0)
        start_time = (double)boot + (double)ticks / hz;
#endif
    char owner[128];
    const int owner_size = snprintf(owner, sizeof owner, "{\"pid\":%lld,\"startTime\":%.6f}",
                                    (long long)getpid(), start_time);
    if (owner_size > 0 && owner_size < (int)sizeof owner) {
        ftruncate(fd, 0);
        if (pwrite(fd, owner, (size_t)owner_size, 0) == owner_size)
            fsync(fd);
    }
    return new OsFileLock{fd};
}

void os_file_unlock(OsFileLock *lock) {
    if (lock) {
        close(lock->fd);
        delete lock;
    }
}

int os_mkdtemp(char *template_path) {
    return mkdtemp(template_path) ? 0 : -1;
}

const char *os_temp_dir(void) {
    const char *t = getenv("TMPDIR");
    static char buf[4096];
    if (!t || !*t)
        return "/tmp";
    size_t n = strlen(t);
    if (n >= sizeof buf)
        return "/tmp";
    memcpy(buf, t, n + 1);
    while (n > 1 && buf[n - 1] == '/')
        buf[--n] = 0;
    return buf;
}

const char *os_null_device(void) {
    return "/dev/null";
}

int os_free_space(const char *path, uint64_t *bytes_out) {
    struct statvfs vfs;
    if (!bytes_out || statvfs(path, &vfs) != 0)
        return -1;
    *bytes_out = uint64_t(vfs.f_bavail) * uint64_t(vfs.f_frsize);
    return 0;
}
int os_set_mtime(const char *path, int64_t mtime) {
    struct timeval times[2];
    times[0].tv_sec = times[1].tv_sec = (time_t)mtime;
    times[0].tv_usec = times[1].tv_usec = 0;
    return utimes(path, times);
}

OsDirectoryWatch *os_directory_watch_create(const char *path, int subtree, uint32_t filter) {
#if defined(__APPLE__) && TARGET_OS_OSX
    if (filter != 0x10) {
        errno = ENOTSUP;
        return nullptr;
    }
    OsStat st{};
    if (os_stat(path, &st) != 0)
        return nullptr;
    if (!st.is_dir) {
        errno = ENOTDIR;
        return nullptr;
    }
    char *canonical = realpath(path, nullptr);
    if (!canonical)
        return nullptr;
    auto *watch = new OsDirectoryWatch;
    watch->path = canonical;
    free(canonical);
    if (watch->path.back() != '/')
        watch->path += '/';
    watch->subtree = subtree != 0;
    CFStringRef name =
        CFStringCreateWithCString(nullptr, watch->path.c_str(), kCFStringEncodingUTF8);
    CFArrayRef paths = name ? CFArrayCreate(nullptr, reinterpret_cast<const void **>(&name), 1,
                                            &kCFTypeArrayCallBacks)
                            : nullptr;
    FSEventStreamContext context{0, watch, nullptr, nullptr, nullptr};
    if (paths)
        watch->stream = FSEventStreamCreate(
            nullptr, directory_watch_events, &context, paths, kFSEventStreamEventIdSinceNow, 0.01,
            kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagNoDefer);
    if (paths)
        CFRelease(paths);
    if (name)
        CFRelease(name);
    watch->queue = dispatch_queue_create("recomp.directory-watch", DISPATCH_QUEUE_SERIAL);
    if (watch->stream && watch->queue) {
        FSEventStreamSetDispatchQueue(watch->stream, watch->queue);
        if (FSEventStreamStart(watch->stream))
            return watch;
    }
    os_directory_watch_close(watch);
    errno = EIO;
    return nullptr;
#else
    (void)path;
    (void)subtree;
    (void)filter;
    errno = ENOSYS;
    return nullptr;
#endif
}

int os_directory_watch_poll(OsDirectoryWatch *watch) {
#if defined(__APPLE__) && TARGET_OS_OSX
    if (watch->overflow.load()) {
        errno = EOVERFLOW;
        return -1;
    }
    return watch->pending.exchange(false) ? 1 : 0;
#else
    (void)watch;
    errno = ENOSYS;
    return -1;
#endif
}

int os_directory_watch_next(OsDirectoryWatch *watch) {
#if defined(__APPLE__) && TARGET_OS_OSX
    return watch->overflow.load() ? -1 : 0;
#else
    (void)watch;
    errno = ENOSYS;
    return -1;
#endif
}

void os_directory_watch_close(OsDirectoryWatch *watch) {
#if defined(__APPLE__) && TARGET_OS_OSX
    if (watch->stream) {
        FSEventStreamStop(watch->stream);
        FSEventStreamInvalidate(watch->stream);
        FSEventStreamRelease(watch->stream);
    }
    if (watch->queue) {
        dispatch_sync_f(watch->queue, nullptr, [](void *) {});
        dispatch_release(watch->queue);
    }
    delete watch;
#else
    (void)watch;
#endif
}

int os_registry_read(const char *, const char *, char *, size_t) {
    return -1;
}
int os_user_data_dir(const char *app, char *buf, size_t cap) {
    const char *home = getenv("HOME");
    char base[4096];
#ifdef __APPLE__
    if (!home || !*home)
        return -1;
    snprintf(base, sizeof base, "%s/Library/Application Support", home);
#else
    const char *xdg = getenv("XDG_DATA_HOME");
    if (xdg && *xdg)
        snprintf(base, sizeof base, "%s", xdg);
    else if (home && *home)
        snprintf(base, sizeof base, "%s/.local/share", home);
    else
        return -1;
#endif
    int n = snprintf(buf, cap, "%s/%s", base, app);
    return n > 0 && (size_t)n < cap ? 0 : -1;
}

extern "C" char **environ;

int os_spawn(const char *const argv[], int64_t *pid_out) {
    pid_t pid;
    if (posix_spawn(&pid, argv[0], NULL, NULL, (char *const *)argv, environ) != 0)
        return -1;
    *pid_out = pid;
    return 0;
}

int os_wait(int64_t pid, int *exit_code) {
    int status = 0;
    if (waitpid((pid_t)pid, &status, 0) < 0)
        return -1;
    if (WIFEXITED(status))
        *exit_code = WEXITSTATUS(status);
    else if (WIFSIGNALED(status))
        *exit_code = 128 + WTERMSIG(status);
    else
        *exit_code = -1;
    return 0;
}

struct OsChildProcess {
    pid_t pid;
    bool exited = false;
    uint32_t exit_code = 0;
};

OsChildProcess *os_child_spawn(const char *const argv[]) {
    pid_t pid;
    posix_spawn_file_actions_t actions;
    posix_spawnattr_t attributes;
    int error = posix_spawn_file_actions_init(&actions);
    if (error) {
        errno = error;
        return nullptr;
    }
    error = posix_spawnattr_init(&attributes);
    if (error) {
        posix_spawn_file_actions_destroy(&actions);
        errno = error;
        return nullptr;
    }
#ifdef __APPLE__
    error = posix_spawnattr_setflags(&attributes, POSIX_SPAWN_CLOEXEC_DEFAULT);
    for (int fd = 0; fd < 3 && !error; ++fd)
        if (fcntl(fd, F_GETFD) >= 0)
            error = posix_spawn_file_actions_addinherit_np(&actions, fd);
#elif defined(__GLIBC__)
#if __GLIBC_PREREQ(2, 34)
    error = posix_spawn_file_actions_addclosefrom_np(&actions, 3);
#else
    error = ENOTSUP;
#endif
#else
    error = ENOTSUP;
#endif
    if (!error)
        error = posix_spawn(&pid, argv[0], &actions, &attributes, (char *const *)argv, environ);
    posix_spawnattr_destroy(&attributes);
    posix_spawn_file_actions_destroy(&actions);
    if (error) {
        errno = error;
        return nullptr;
    }
    return new OsChildProcess{pid};
}

uint32_t os_child_id(OsChildProcess *child, int) {
    return (uint32_t)child->pid;
}

int os_child_poll(OsChildProcess *child, int primary_thread, uint32_t *exit_code) {
    if (primary_thread)
        return OS_CHILD_THREAD_UNSUPPORTED;
    if (!child->exited) {
        int status;
        pid_t result;
        do {
            result = waitpid(child->pid, &status, WNOHANG);
        } while (result < 0 && errno == EINTR);
        if (result <= 0)
            return result < 0 ? OS_CHILD_ERROR : OS_CHILD_RUNNING;
        child->exited = true;
        child->exit_code = WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
    }
    *exit_code = child->exit_code;
    return OS_CHILD_EXITED;
}

static void *reap_child(void *value) {
    auto *child = static_cast<OsChildProcess *>(value);
    while (waitpid(child->pid, nullptr, 0) < 0 && errno == EINTR) {
    }
    delete child;
    return nullptr;
}

void os_child_close(OsChildProcess *child) {
    uint32_t code;
    if (os_child_poll(child, 0, &code) != 0) {
        delete child;
        return;
    }
    OsThread *thread = os_thread_create(reap_child, child, 0);
    if (thread)
        os_thread_detach(thread);
    else {
        fprintf(stderr, "os_child_close: cannot create child reaper\n");
        delete child;
    }
}

static int native_flags(int flags) {
    int f = 0;
    switch (flags & 3) {
    case OS_O_WRONLY:
        f = O_WRONLY;
        break;
    case OS_O_RDWR:
        f = O_RDWR;
        break;
    default:
        f = O_RDONLY;
        break;
    }
    if (flags & OS_O_CREAT)
        f |= O_CREAT;
    if (flags & OS_O_EXCL)
        f |= O_EXCL;
    if (flags & OS_O_TRUNC)
        f |= O_TRUNC;
    return f;
}
int os_fd_open(const char *path, int flags) {
    return open(path, native_flags(flags), 0644);
}
int64_t os_fd_read(int fd, void *buf, size_t n) {
    return (int64_t)read(fd, buf, n);
}
int64_t os_fd_write(int fd, const void *buf, size_t n) {
    return (int64_t)write(fd, buf, n);
}
int64_t os_fd_seek(int fd, int64_t off, int whence) {
    return (int64_t)lseek(fd, (off_t)off, whence);
}
int os_fd_close(int fd) {
    return close(fd);
}
int os_fd_dup(int fd) {
    return dup(fd);
}
int os_fd_fsync(int fd) {
    return fsync(fd);
}
int os_fd_truncate(int fd, int64_t length) {
    return ftruncate(fd, (off_t)length);
}
int os_fd_stat(int fd, OsStat *out) {
    struct stat st;
    if (fstat(fd, &st) != 0)
        return -1;
    fill_stat(st, out);
    return 0;
}
void *os_fdopen(int fd, const char *mode) {
    return fdopen(fd, mode);
}

int os_exe_path(char *buf, size_t cap) {
#ifdef __EMSCRIPTEN__
    // The web build's files are packaged under /app: resources in /app/resources.
    snprintf(buf, cap, "/app/app");
    return 0;
#elif defined(__APPLE__)
    uint32_t size = (uint32_t)cap;
    return _NSGetExecutablePath(buf, &size) == 0 ? 0 : -1;
#else
    ssize_t n = readlink("/proc/self/exe", buf, cap - 1);
    if (n < 0)
        return -1;
    buf[n] = 0;
    return 0;
#endif
}

static OsFaultFn g_fault_fn = nullptr;
static volatile uint64_t g_fault_addr = 0;
uint64_t os_fault_address(void) {
    return g_fault_addr;
}
static void fault_trampoline(int sig, siginfo_t *info, void *) {
    g_fault_addr =
        (sig == SIGSEGV || sig == SIGBUS) && info ? (uint64_t)(uintptr_t)info->si_addr : 0;
    const char *what = sig == SIGSEGV   ? "SIGSEGV"
                       : sig == SIGBUS  ? "SIGBUS"
                       : sig == SIGABRT ? "an abort from the runtime"
                                        : "a fatal signal";
    g_fault_fn(what);
}
int os_install_fault_handlers(OsFaultFn fn) {
    g_fault_fn = fn;
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = fault_trampoline;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, nullptr);
    sigaction(SIGBUS, &sa, nullptr);
    sigaction(SIGABRT, &sa, nullptr);
    return 1;
}

void os_write_stderr_raw(const char *s, size_t n) {
    ssize_t ignored = write(2, s, n);
    (void)ignored;
}
void os_exit_immediately(int code) {
    _exit(code);
}

int os_localtime(int64_t seconds, struct tm *out) {
    time_t t = (time_t)seconds;
    return localtime_r(&t, out) ? 0 : -1;
}
int os_gmtime(int64_t seconds, struct tm *out) {
    time_t t = (time_t)seconds;
    return gmtime_r(&t, out) ? 0 : -1;
}

int64_t os_process_id(void) {
    return (int64_t)getpid();
}

uint64_t os_monotonic_ns(void) {
#ifdef __APPLE__
    // CLOCK_MONOTONIC on Darwin works out the boot time on every call
    // (gettimeofday); the uptime clock is a plain mach_absolute_time read.
    // Every import call reads this clock, so the difference is large.
    return clock_gettime_nsec_np(CLOCK_UPTIME_RAW);
#endif
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}
uint64_t os_wall_time_us(void) {
    struct timeval tv;
    gettimeofday(&tv, nullptr);
    return (uint64_t)tv.tv_sec * 1000000ull + (uint64_t)tv.tv_usec;
}
void os_sleep_us(uint64_t us) {
    struct timespec ts;
    ts.tv_sec = (time_t)(us / 1000000ull);
    ts.tv_nsec = (long)((us % 1000000ull) * 1000ull);
    nanosleep(&ts, nullptr);
}

int os_strcasecmp(const char *a, const char *b) {
    return strcasecmp(a, b);
}

#if defined(__ANDROID__)
int os_backtrace(int, OsFrameFn, void *) {
    return 0;
}
#else
int os_backtrace(int max_frames, OsFrameFn fn, void *ctx) {
    if (max_frames > 64)
        max_frames = 64;
    if (max_frames <= 0)
        return 0;
    void *frames[64];
    const int n = backtrace(frames, max_frames);
    char **names = backtrace_symbols(frames, n);
    if (!names)
        return 0;
    for (int i = 0; i < n; ++i)
        fn(names[i], ctx);
    free(names);
    return n;
}
#endif

int os_compare_utf16(uint32_t lcid, uint32_t flags, const uint16_t *a, int a_len, const uint16_t *b,
                     int b_len) {
#ifdef __APPLE__
    if ((lcid != 0x0400 && lcid != 0x0409 && lcid != 0x0800) || a_len < 0 || b_len < 0 ||
        (a_len && !a) || (b_len && !b)) {
        errno = EINVAL;
        return 0;
    }
    static const uint16_t empty = 0;
    if (!a)
        a = &empty;
    if (!b)
        b = &empty;
    constexpr uint32_t supported = 0x1 | 0x2 | 0x20000;
    if (flags & ~supported) {
        errno = ENOSYS;
        return 0;
    }
    CFStringCompareFlags options = 0;
    if (flags & 0x1)
        options |= kCFCompareCaseInsensitive;
    if (flags & 0x2)
        options |= kCFCompareDiacriticInsensitive;
    if (flags & 0x20000)
        options |= kCFCompareWidthInsensitive;
    CFStringRef left = CFStringCreateWithCharacters(kCFAllocatorDefault, (const UniChar *)a, a_len);
    CFStringRef right =
        CFStringCreateWithCharacters(kCFAllocatorDefault, (const UniChar *)b, b_len);
    CFLocaleRef locale = CFLocaleCreate(kCFAllocatorDefault, CFSTR("en_US"));
    if (!left || !right || !locale) {
        if (left)
            CFRelease(left);
        if (right)
            CFRelease(right);
        if (locale)
            CFRelease(locale);
        errno = ENOMEM;
        return 0;
    }
    CFComparisonResult result =
        CFStringCompareWithOptionsAndLocale(left, right, CFRangeMake(0, a_len), options, locale);
    CFRelease(left);
    CFRelease(right);
    CFRelease(locale);
    return result < 0 ? 1 : result == 0 ? 2 : 3;
#else
    (void)lcid;
    (void)flags;
    (void)a;
    (void)a_len;
    (void)b;
    (void)b_len;
    errno = ENOSYS;
    return 0;
#endif
}

int os_hostname(char *buf, size_t cap) {
    if (!buf || cap == 0 || gethostname(buf, cap) != 0)
        return -1;
    buf[cap - 1] = '\0';
    return 0;
}

int os_resolve_ipv4(const char *name, unsigned char addr[4]) {
    if (!name || !*name)
        return -1;
    struct addrinfo hints;
    memset(&hints, 0, sizeof hints);
    hints.ai_family = AF_INET; // IPv4 only: the guest's hostent is AF_INET
    struct addrinfo *res = nullptr;
    if (getaddrinfo(name, nullptr, &hints, &res) != 0 || !res)
        return -1;
    memcpy(addr, &((struct sockaddr_in *)res->ai_addr)->sin_addr, 4);
    freeaddrinfo(res);
    return 0;
}

static thread_local int t_socket_error = 0;

static int socket_wsa_error(int err) {
    switch (err) {
    case EWOULDBLOCK:
        return 10035;
    case EINPROGRESS:
        return 10036;
    case EALREADY:
        return 10037;
    case ENOTSOCK:
    case EBADF:
        return 10038;
    case EDESTADDRREQ:
        return 10039;
    case EMSGSIZE:
        return 10040;
    case ENOPROTOOPT:
        return 10042;
    case EPROTONOSUPPORT:
        return 10043;
    case EOPNOTSUPP:
        return 10045;
    case EAFNOSUPPORT:
        return 10047;
    case EADDRINUSE:
        return 10048;
    case EADDRNOTAVAIL:
        return 10049;
    case ENETDOWN:
        return 10050;
    case ENETUNREACH:
        return 10051;
    case ECONNABORTED:
        return 10053;
    case ECONNRESET:
    case EPIPE:
        return 10054;
    case ENOBUFS:
    case ENOMEM:
        return 10055;
    case EISCONN:
        return 10056;
    case ENOTCONN:
        return 10057;
    case ESHUTDOWN:
        return 10058;
    case ETIMEDOUT:
        return 10060;
    case ECONNREFUSED:
        return 10061;
    case EHOSTDOWN:
        return 10064;
    case EHOSTUNREACH:
        return 10065;
    case EACCES:
        return 10013;
    case EFAULT:
        return 10014;
    case EMFILE:
    case ENFILE:
        return 10024;
    default:
        return 10022;
    }
}

static int socket_failed() {
    t_socket_error = socket_wsa_error(errno);
    return -1;
}

static struct sockaddr_in socket_address(uint32_t addr, uint16_t port) {
    struct sockaddr_in sa;
    memset(&sa, 0, sizeof sa);
#ifdef __APPLE__
    sa.sin_len = sizeof sa;
#endif
    sa.sin_family = AF_INET;
    sa.sin_addr.s_addr = addr;
    sa.sin_port = port;
    return sa;
}

static void socket_address_out(const struct sockaddr_in &sa, uint32_t *addr, uint16_t *port) {
    if (addr)
        *addr = sa.sin_addr.s_addr;
    if (port)
        *port = sa.sin_port;
}

static int socket_prepare(int fd) {
    int flags = fcntl(fd, F_GETFL, 0);
    if (flags < 0 || fcntl(fd, F_SETFL, flags | O_NONBLOCK) != 0 ||
        fcntl(fd, F_SETFD, FD_CLOEXEC) != 0)
        return -1;
#ifdef SO_NOSIGPIPE
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
#endif
    return 0;
}

OsSocket os_socket_open(int stream) {
    int fd = socket(AF_INET, stream ? SOCK_STREAM : SOCK_DGRAM, stream ? IPPROTO_TCP : IPPROTO_UDP);
    if (fd < 0)
        return socket_failed();
    if (socket_prepare(fd) != 0) {
        socket_failed();
        close(fd);
        return OS_SOCKET_INVALID;
    }
    return fd;
}

int os_socket_close(OsSocket s) {
    return close((int)s) == 0 ? 0 : socket_failed();
}

int os_socket_bind(OsSocket s, uint32_t addr, uint16_t port) {
    struct sockaddr_in sa = socket_address(addr, port);
    return bind((int)s, (struct sockaddr *)&sa, sizeof sa) == 0 ? 0 : socket_failed();
}

int os_socket_connect(OsSocket s, uint32_t addr, uint16_t port) {
    struct sockaddr_in sa = socket_address(addr, port);
    if (connect((int)s, (struct sockaddr *)&sa, sizeof sa) == 0)
        return 0;
    if (errno == EINPROGRESS) {
        t_socket_error = 10035;
        return -1;
    }
    return socket_failed();
}

int os_socket_listen(OsSocket s, int backlog) {
    return listen((int)s, backlog) == 0 ? 0 : socket_failed();
}

OsSocket os_socket_accept(OsSocket s, uint32_t *addr, uint16_t *port) {
    struct sockaddr_in sa;
    socklen_t len = sizeof sa;
    int fd = accept((int)s, (struct sockaddr *)&sa, &len);
    if (fd < 0)
        return socket_failed();
    if (socket_prepare(fd) != 0) {
        socket_failed();
        close(fd);
        return OS_SOCKET_INVALID;
    }
    socket_address_out(sa, addr, port);
    return fd;
}

int os_socket_shutdown(OsSocket s, int how) {
    int h = how == 0 ? SHUT_RD : how == 1 ? SHUT_WR : SHUT_RDWR;
    return shutdown((int)s, h) == 0 ? 0 : socket_failed();
}

static int socket_flags(int flags) {
    return (flags & 1 ? MSG_OOB : 0) | (flags & 2 ? MSG_PEEK : 0) | (flags & 4 ? MSG_DONTROUTE : 0);
}

int os_socket_send(OsSocket s, const void *buf, size_t len, int flags, const uint32_t *to_addr,
                   uint16_t to_port) {
    ssize_t n;
    int f = socket_flags(flags);
#ifdef MSG_NOSIGNAL
    f |= MSG_NOSIGNAL;
#endif
    if (to_addr) {
        struct sockaddr_in sa = socket_address(*to_addr, to_port);
        n = sendto((int)s, buf, len, f, (struct sockaddr *)&sa, sizeof sa);
    } else {
        n = send((int)s, buf, len, f);
    }
    return n < 0 ? socket_failed() : (int)n;
}

int os_socket_recv(OsSocket s, void *buf, size_t len, int flags, uint32_t *from_addr,
                   uint16_t *from_port) {
    struct sockaddr_in sa;
    memset(&sa, 0, sizeof sa);
    struct iovec iov = {buf, len};
    struct msghdr msg;
    memset(&msg, 0, sizeof msg);
    msg.msg_name = &sa;
    msg.msg_namelen = sizeof sa;
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    ssize_t n = recvmsg((int)s, &msg, socket_flags(flags));
    if (n < 0)
        return socket_failed();
    socket_address_out(sa, from_addr, from_port);
    if (msg.msg_flags & MSG_TRUNC) {
        t_socket_error = 10040;
        return -1;
    }
    return (int)n;
}

static bool socket_option_name(enum OsSocketOption option, int *level, int *name) {
    *level = SOL_SOCKET;
    switch (option) {
    case OS_SOCKOPT_BROADCAST:
        *name = SO_BROADCAST;
        return true;
    case OS_SOCKOPT_REUSEADDR:
        *name = SO_REUSEADDR;
        return true;
    case OS_SOCKOPT_KEEPALIVE:
        *name = SO_KEEPALIVE;
        return true;
    case OS_SOCKOPT_RCVBUF:
        *name = SO_RCVBUF;
        return true;
    case OS_SOCKOPT_SNDBUF:
        *name = SO_SNDBUF;
        return true;
    case OS_SOCKOPT_ERROR:
        *name = SO_ERROR;
        return true;
    case OS_SOCKOPT_TYPE:
        *name = SO_TYPE;
        return true;
    case OS_SOCKOPT_NODELAY:
        *level = IPPROTO_TCP;
        *name = TCP_NODELAY;
        return true;
    default:
        return false;
    }
}

int os_socket_set_option(OsSocket s, enum OsSocketOption option, int value) {
    if (option == OS_SOCKOPT_LINGER) {
        struct linger l = {value >= 0, value >= 0 ? value : 0};
        return setsockopt((int)s, SOL_SOCKET, SO_LINGER, &l, sizeof l) == 0 ? 0 : socket_failed();
    }
    int level, name;
    if (!socket_option_name(option, &level, &name) || option == OS_SOCKOPT_ERROR ||
        option == OS_SOCKOPT_TYPE) {
        t_socket_error = 10042;
        return -1;
    }
    return setsockopt((int)s, level, name, &value, sizeof value) == 0 ? 0 : socket_failed();
}

int os_socket_get_option(OsSocket s, enum OsSocketOption option, int *value) {
    if (option == OS_SOCKOPT_LINGER) {
        struct linger l;
        socklen_t len = sizeof l;
        if (getsockopt((int)s, SOL_SOCKET, SO_LINGER, &l, &len) != 0)
            return socket_failed();
        *value = l.l_onoff ? l.l_linger : -1;
        return 0;
    }
    int level, name;
    if (!socket_option_name(option, &level, &name)) {
        t_socket_error = 10042;
        return -1;
    }
    socklen_t len = sizeof *value;
    if (getsockopt((int)s, level, name, value, &len) != 0)
        return socket_failed();
    if (option == OS_SOCKOPT_ERROR && *value)
        *value = socket_wsa_error(*value);
    if (option == OS_SOCKOPT_TYPE)
        *value = *value == SOCK_STREAM ? 1 : 2;
    return 0;
}

int os_socket_name(OsSocket s, int peer, uint32_t *addr, uint16_t *port) {
    struct sockaddr_in sa;
    socklen_t len = sizeof sa;
    int r = peer ? getpeername((int)s, (struct sockaddr *)&sa, &len)
                 : getsockname((int)s, (struct sockaddr *)&sa, &len);
    if (r != 0)
        return socket_failed();
    socket_address_out(sa, addr, port);
    return 0;
}

int os_socket_readable_bytes(OsSocket s, uint32_t *bytes) {
    int n = 0;
    if (ioctl((int)s, FIONREAD, &n) != 0)
        return socket_failed();
    *bytes = (uint32_t)n;
    return 0;
}

int os_socket_poll(const OsSocket *s, const int *want, int *got, int count, int timeout_ms) {
    std::vector<struct pollfd> fds((size_t)count);
    for (int i = 0; i < count; ++i) {
        fds[i].fd = (int)s[i];
        fds[i].events = (short)((want[i] & OS_POLL_READ ? POLLIN : 0) |
                                (want[i] & OS_POLL_WRITE ? POLLOUT : 0) |
                                (want[i] & OS_POLL_ERROR ? POLLPRI : 0));
        fds[i].revents = 0;
    }
    int r;
    do {
        r = poll(fds.data(), (nfds_t)count, timeout_ms);
    } while (r < 0 && errno == EINTR);
    if (r < 0)
        return socket_failed();
    int ready = 0;
    for (int i = 0; i < count; ++i) {
        short ev = fds[i].revents;
        if (ev & POLLNVAL) {
            t_socket_error = 10038;
            return -1;
        }
        int g = 0;
        if (ev & (POLLIN | POLLHUP | POLLERR))
            g |= OS_POLL_READ;
        if (ev & (POLLOUT | POLLERR | POLLHUP))
            g |= OS_POLL_WRITE;
        if (ev & (POLLPRI | POLLERR))
            g |= OS_POLL_ERROR;
        got[i] = g & want[i];
        ready += got[i] != 0;
    }
    return ready;
}

int os_primary_ipv4(unsigned char addr[4]) {
    int fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    if (fd < 0)
        return -1;
    struct sockaddr_in sa = socket_address(htonl(0x08080808u), htons(53));
    socklen_t len = sizeof sa;
    int r = connect(fd, (struct sockaddr *)&sa, sizeof sa) == 0 &&
                    getsockname(fd, (struct sockaddr *)&sa, &len) == 0 && sa.sin_addr.s_addr != 0
                ? 0
                : -1;
    close(fd);
    if (r == 0)
        memcpy(addr, &sa.sin_addr, 4);
    return r;
}

int os_socket_error(void) {
    return t_socket_error;
}

const char *os_getenv(const char *name) {
    return getenv(name);
}

int os_setenv(const char *name, const char *value) {
    return setenv(name, value, 1);
}
int os_unsetenv(const char *name) {
    return unsetenv(name);
}

} // extern "C"
