// Internal ANSI/W implementation seam. Names are UTF-8; addresses stay guest values.
#pragma once
#include "imports.h"
#include <string>

void kernel32_wide_register();
void kernel32_exit_process(uint32_t code);
void kernel32_exit_thread(X86 *c);
std::string current_directory();
void set_current_directory_named(X86 *c, const std::string &name);
void create_file_named(X86 *c, const std::string &name);
void get_file_attributes_named(X86 *c, const std::string &name);
void set_file_attributes_named(X86 *c, const std::string &name);
void create_directory_named(X86 *c, const std::string &name);
void remove_directory_named(X86 *c, const std::string &name);
void delete_file_named(X86 *c, const std::string &name);
void move_file_named(X86 *c, const std::string &source, const std::string &dest);
void move_file_ex_named(X86 *c, const std::string &source, const std::string &dest, uint32_t flags);
void copy_file_named(X86 *c, const std::string &source, const std::string &dest);
void find_first_named(X86 *c, const std::string &pattern, bool wide);
void find_next(X86 *c, bool wide);
uint32_t win32_crt_findfirst(const std::string &pattern, uint32_t data);
bool win32_crt_findnext(uint32_t handle, uint32_t data);
bool win32_crt_findclose(uint32_t handle);
std::string full_path_named(const std::string &name);
void volume_information_named(X86 *c, const std::string &root, bool wide);
void disk_free_space(X86 *c);
void drive_type_named(X86 *c, const std::string &root);
void logical_drive_strings(X86 *c, bool wide);
void get_file_attributes_ex_named(X86 *c, const std::string &name);

void create_event_named(X86 *c, const std::string &name);
void create_mutex_named(X86 *c, const std::string &name);
void create_mapping_named(X86 *c, const std::string &name);
void open_mutex_named(X86 *c, const std::string &name);

void kernel32_wide_reset();

uint32_t kernel32_create_guest_thread(X86 *c, uint32_t start, uint32_t param, uint32_t flags,
                                      uint32_t ptid);

bool kernel32_guest_environment_lookup(const std::string &name, std::string &value);

void get_command_line(X86 *c);
uint32_t win32_command_line();
void startup_info(X86 *c);
void wait_multiple_objects(X86 *c);
void kernel32_wide_reset_command_line();

// GetTimeFormatA/W dwFlags (winnt.h TIME_* values). Declared once so the ANSI
// and wide shims interpret the same picture syntax.
enum Kernel32TimeFlag {
    K32_TIME_NOMINUTESORSECONDS = 0x00000001,
    K32_TIME_NOSECONDS = 0x00000002,
    K32_TIME_NOTIMEMARKER = 0x00000004,
    K32_TIME_FORCE24HOURFORMAT = 0x00000008,
};

// Formats the time-of-day fields using the subset of the Windows time picture
// syntax the engine can encounter. `picture` is UTF-8 and empty selects the
// runtime's fixed en-US default, "h:mm:ss tt". `hour` is 0..23 and the other
// fields are already range-checked by the caller. Shared by the ANSI and wide
// GetTimeFormat shims so both write identical text.
std::string kernel32_format_time(int hour, int minute, int second, const std::string &picture,
                                 uint32_t flags);

// Shared GetTimeFormatA/W input, validation, size query and buffer handling.
void kernel32_get_time_format(X86 *c, bool wide);
