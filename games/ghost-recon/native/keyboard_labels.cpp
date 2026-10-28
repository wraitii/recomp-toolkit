#include "keyboard_labels.h"
#include "key_names.h"
#include "memory.h"

#include <cstdio>
#include <cstdlib>
#include <string_view>

extern "C" void fn_0056fd90(X86 *c);

namespace {
uint32_t label_scancode(std::string_view key) {
    if (key.substr(0, 4) != "key_")
        return 0;
    key.remove_prefix(4);
    if (key.size() == 1) {
        constexpr uint8_t letters[] = {0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17,
                                       0x24, 0x25, 0x26, 0x32, 0x31, 0x18, 0x19, 0x10, 0x13,
                                       0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c};
        if (key[0] >= 'a' && key[0] <= 'z')
            return letters[key[0] - 'a'];
        if (key[0] >= '1' && key[0] <= '9')
            return key[0] - '1' + 2;
        if (key[0] == '0')
            return 0x0b;
    }
    constexpr struct {
        std::string_view key;
        uint8_t scancode;
    } keys[] = {
        {"-", 0x0c},
        {"=", 0x0d},
        {"backspace", 0x0e},
        {"tab", 0x0f},
        {"[", 0x1a},
        {"]", 0x1b},
        {"return", 0x1c},
        {"lcontrol", 0x1d},
        {";", 0x27},
        {"'", 0x28},
        {"`", 0x29},
        {"lshift", 0x2a},
        {"backslash", 0x2b},
        {"comma", 0x33},
        {".", 0x34},
        {"slash", 0x35},
        {"rshift", 0x36},
        {"numpad_multiply", 0x37},
        {"lalt", 0x38},
        {"space", 0x39},
        {"caps", 0x3a},
        {"f1", 0x3b},
        {"f2", 0x3c},
        {"f3", 0x3d},
        {"f4", 0x3e},
        {"f5", 0x3f},
        {"f6", 0x40},
        {"f7", 0x41},
        {"f8", 0x42},
        {"f9", 0x43},
        {"f10", 0x44},
        {"numlock", 0x45},
        {"scroll", 0x46},
        {"numpad7", 0x47},
        {"numpad8", 0x48},
        {"numpad9", 0x49},
        {"numpad_minus", 0x4a},
        {"numpad4", 0x4b},
        {"numpad5", 0x4c},
        {"numpad6", 0x4d},
        {"numpad+plus", 0x4e},
        {"numpad1", 0x4f},
        {"numpad2", 0x50},
        {"numpad3", 0x51},
        {"numpad0", 0x52},
        {"numpad_decimal", 0x53},
        {"f11", 0x57},
        {"f12", 0x58},
        {"numpad_enter", 0x9c},
        {"rcontrol", 0x9d},
        {"numpad_divide", 0xb5},
        {"sysrq", 0xb7},
        {"ralt", 0xb8},
        {"pause", 0xc5},
        {"home", 0xc7},
        {"up", 0xc8},
        {"pageup", 0xc9},
        {"left", 0xcb},
        {"right", 0xcd},
        {"end", 0xcf},
        {"down", 0xd0},
        {"pagedown", 0xd1},
        {"insert", 0xd2},
        {"delete", 0xd3},
    };
    for (const auto &entry : keys)
        if (entry.key == key)
            return entry.scancode;
    return 0;
}
} // namespace

extern "C" void gr_localized_key_label(X86 *c) {
    std::string name;
    if (guest_key_names_system()) {
        const uint32_t key = rd32(c->r[R_ESP] + 8);
        if (gm_valid(key, 4) && rd32(key) == 0x5f79656b) {
            const uint32_t scancode = label_scancode(gm_str(key));
            if (scancode)
                name = guest_key_name(scancode);
        }
    }
    fn_0056fd90(c);
    if (name.empty())
        return;
    const X86 saved = *c;
    const uint32_t bytes = uint32_t((name.size() + 1) * 2);
    const uint32_t sp = (saved.r[R_ESP] - bytes - 8) & ~15u;
    if (!gm_valid(sp, saved.r[R_ESP] - sp) || !gm_valid(saved.r[R_EAX], 8)) {
        fprintf(stderr, "keyboard labels: invalid guest string scratch or output\n");
        std::abort();
    }
    const uint32_t text = sp + 8;
    gm_put_wstr(text, name, uint32_t(name.size() + 1));
    c->r[R_ESP] = text;
    c->r[R_ECX] = saved.r[R_EAX];
    recomp_guest_call(c, 0x0056c050, &text, 1);
    *c = saved;
}
