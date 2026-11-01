// run_main.cpp - recomp-run: runs an arbitrary PE32 program through the
// interpreter on the headless host, uncapped, writing no frames, and exits
// with the guest's own exit code.
#define main headless_main
#include "headless_main.cpp"
#undef main

namespace {

// Quote each argument so the guest CRT's MSVC split gives it back unchanged.
std::string guest_command_line(int argc, char **argv) {
    std::string args;
    for (int i = 2; i < argc; ++i) {
        if (!args.empty())
            args += ' ';
        args += '"';
        unsigned backslashes = 0;
        for (const char *p = argv[i]; *p; ++p) {
            if (*p == '\\') {
                ++backslashes;
                continue;
            }
            args.append(backslashes * (*p == '"' ? 2 : 1), '\\');
            backslashes = 0;
            if (*p == '"')
                args += '\\';
            args += *p;
        }
        args.append(backslashes * 2, '\\');
        args += '"';
    }
    return args;
}

} // namespace

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: recomp-run /path/to/program.exe [arguments...]\n");
        return 2;
    }
    os_setenv("RECOMP_GUEST_ARGS", guest_command_line(argc, argv).c_str());
    g_max_frames = 0;
    g_max_seconds = 0;
    g_frame_every = 0;

    BootOptions opts;
    opts.name = "recomp-run";
    opts.exe = argv[1];
    opts.interpreted = true;
    opts.load_mods = false;
    if (!headless_run(opts))
        return 2;

    if (process_exited() && !boot_forced_stop())
        return (int)process_exit_code();
    if (boot_abnormal_exit()) {
        fprintf(stderr, "recomp-run: the run did not end normally (%s)\n", boot_stop_reason());
        return 3;
    }
    return process_exited() ? (int)process_exit_code() : 0;
}
