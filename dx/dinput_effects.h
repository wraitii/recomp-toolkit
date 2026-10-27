struct DiEffect {
    uint32_t device = 0, type = 0, duration = 0, gain = 10000, period = 0;
    int32_t magnitude = 0, offset = 0;
    uint64_t started = 0, ends = 0;
    bool playing = false;
};

constexpr uint32_t kConstantForce = 0x13541c20u;
constexpr uint32_t kRampForce = 0x13541c21u;
constexpr uint32_t kSquare = 0x13541c22u;
constexpr uint32_t kSine = 0x13541c23u;
constexpr uint32_t kTriangle = 0x13541c24u;
constexpr uint32_t kSawUp = 0x13541c25u;
constexpr uint32_t kSawDown = 0x13541c26u;
constexpr uint32_t kEffectDuration = 0x001u;
constexpr uint32_t kEffectGain = 0x004u;
constexpr uint32_t kEffectTypeParameters = 0x100u;

std::mutex g_effect_mutex;
std::map<uint32_t, DiEffect> g_effects;
uint16_t g_effect_level = 0;

bool effect_parameters(DiEffect &effect, uint32_t at, uint32_t flags) {
    if (!at || !gm_valid(at, 52) || rd32(at) < 52)
        return false;
    if (flags & kEffectDuration)
        effect.duration = rd32(at + 8);
    if (flags & kEffectGain)
        effect.gain = std::min(rd32(at + 16), 10000u);
    if (flags & kEffectTypeParameters) {
        uint32_t size = rd32(at + 44), data = rd32(at + 48);
        if (effect.type == kConstantForce) {
            if (size < 4 || !gm_valid(data, 4))
                return false;
            effect.magnitude = (int32_t)rd32(data);
        } else {
            if (size < 16 || !gm_valid(data, 16))
                return false;
            effect.magnitude = (int32_t)rd32(data);
            effect.offset = (int32_t)rd32(data + 4);
            effect.period = rd32(data + 12);
        }
    }
    return true;
}

void effect_stop_device(uint32_t device) {
    std::lock_guard<std::mutex> lock(g_effect_mutex);
    for (auto &entry : g_effects)
        if (entry.second.device == device)
            entry.second.playing = false;
}

void effect_destroy(ComObj *effect) {
    std::lock_guard<std::mutex> lock(g_effect_mutex);
    g_effects.erase(effect->id);
}

void Effect_SetParameters(X86 *c) {
    ComObj *obj = com_this(arg(c, 0), IF_DINPUTEFFECT);
    std::lock_guard<std::mutex> lock(g_effect_mutex);
    auto it = obj ? g_effects.find(obj->id) : g_effects.end();
    if (it == g_effects.end()) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    DiEffect next = it->second;
    if (!effect_parameters(next, arg(c, 1), arg(c, 2))) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    it->second = next;
    com_ret(c, DI_OK);
}

void Effect_Start(X86 *c) {
    ComObj *obj = com_this(arg(c, 0), IF_DINPUTEFFECT);
    std::lock_guard<std::mutex> lock(g_effect_mutex);
    auto it = obj ? g_effects.find(obj->id) : g_effects.end();
    if (it == g_effects.end() || !arg(c, 1)) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    DiEffect &e = it->second;
    e.started = os_monotonic_ns();
    e.ends = e.duration == 0xffffffffu ? UINT64_MAX
                                       : e.started + uint64_t(e.duration) * 1000 * arg(c, 1);
    e.playing = true;
    com_ret(c, DI_OK);
}

void Effect_Stop(X86 *c) {
    ComObj *obj = com_this(arg(c, 0), IF_DINPUTEFFECT);
    std::lock_guard<std::mutex> lock(g_effect_mutex);
    auto it = obj ? g_effects.find(obj->id) : g_effects.end();
    if (it == g_effects.end()) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    it->second.playing = false;
    com_ret(c, DI_OK);
}

void Effect_GetEffectStatus(X86 *c) {
    ComObj *obj = com_this(arg(c, 0), IF_DINPUTEFFECT);
    uint32_t out = arg(c, 1);
    std::lock_guard<std::mutex> lock(g_effect_mutex);
    auto it = obj ? g_effects.find(obj->id) : g_effects.end();
    if (it == g_effects.end() || !out || !gm_valid(out, 4)) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    wr32(out, it->second.playing && os_monotonic_ns() < it->second.ends ? 1 : 0);
    com_ret(c, DI_OK);
}

DX_STUB(Effect_Initialize, DIERR_UNSUPPORTED)
DX_STUB(Effect_GetEffectGuid, DIERR_UNSUPPORTED)
DX_STUB(Effect_GetParameters, DIERR_UNSUPPORTED)
void Effect_Download(X86 *c) {
    com_ret(c, com_this(arg(c, 0), IF_DINPUTEFFECT) ? DI_OK : DIERR_INVALIDPARAM);
}
void Effect_Unload(X86 *c) {
    Effect_Stop(c);
}
DX_STUB(Effect_Escape, DIERR_UNSUPPORTED)

const ComMethod g_effect_methods[] = {
    {"QueryInterface", 3, com_QueryInterface},
    {"AddRef", 1, com_AddRef},
    {"Release", 1, com_Release},
    {"Initialize", 4, Effect_Initialize},
    {"GetEffectGuid", 2, Effect_GetEffectGuid},
    {"GetParameters", 3, Effect_GetParameters},
    {"SetParameters", 3, Effect_SetParameters},
    {"Start", 3, Effect_Start},
    {"Stop", 1, Effect_Stop},
    {"GetEffectStatus", 2, Effect_GetEffectStatus},
    {"Download", 1, Effect_Download},
    {"Unload", 1, Effect_Unload},
    {"Escape", 2, Effect_Escape},
};

void Device_CreateEffect(X86 *c) {
    ComObj *device = this_device(c);
    uint32_t guid = arg(c, 1), params = arg(c, 2), out = arg(c, 3);
    if (!device || !is_joystick(device) || !guid || !out || !gm_valid(guid, 16) ||
        !gm_valid(out, 4) || arg(c, 4)) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    wr32(out, 0);
    static const uint8_t suffix[12] = {0x33, 0x8e, 0xd0, 0x11, 0x9a, 0xd0,
                                       0x00, 0xa0, 0xc9, 0xa0, 0x6e, 0x35};
    uint32_t type = rd32(guid);
    if (memcmp(gm_ptr(guid + 4), suffix, 12) || type < kConstantForce || type > kSawDown ||
        type == kRampForce) {
        char key[80];
        snprintf(key, sizeof(key), "dinput.effect.%08x.%08x.%08x.%08x", type, rd32(guid + 4),
                 rd32(guid + 8), rd32(guid + 12));
        log_once(key, "dinput: unsupported force feedback effect %08x", type);
        com_ret(c, DIERR_UNSUPPORTED);
        return;
    }
    DiEffect effect;
    effect.device = device->id;
    effect.type = type;
    if (!effect_parameters(effect, params, kEffectDuration | kEffectGain | kEffectTypeParameters)) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    ComObj *obj = com_new(K_DINPUTEFFECT);
    uint32_t view = com_view(obj, IF_DINPUTEFFECT);
    if (!view) {
        com_release(obj);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    {
        std::lock_guard<std::mutex> lock(g_effect_mutex);
        g_effects[obj->id] = effect;
    }
    wr32(out, view);
    com_ret(c, DI_OK);
}

void Device_SendForceFeedbackCommand(X86 *c) {
    ComObj *device = this_device(c);
    if (!device || !is_joystick(device)) {
        com_ret(c, DIERR_UNSUPPORTED);
        return;
    }
    switch (arg(c, 1)) {
    case 1:
    case 2:
    case 0x20:
        effect_stop_device(device->id);
        com_ret(c, DI_OK);
        return;
    case 0x10:
        com_ret(c, DI_OK);
        return;
    default:
        LOGW("dinput: unsupported force feedback command %08x", arg(c, 1));
        com_ret(c, DIERR_UNSUPPORTED);
    }
}

void feedback_tick() {
    uint16_t level = 0;
    {
        std::lock_guard<std::mutex> lock(g_effect_mutex);
        const uint64_t now = os_monotonic_ns();
        for (auto &entry : g_effects) {
            DiEffect &e = entry.second;
            if (!e.playing)
                continue;
            if (now >= e.ends) {
                e.playing = false;
                continue;
            }
            // DIVERGENCE(original): directional wheel forces are approximated by gamepad motors.
            double value = std::abs(double(e.magnitude)) / 10000.0;
            if (e.type != kConstantForce) {
                double phase =
                    e.period ? double((now - e.started) / 1000 % e.period) / e.period : 0;
                double wave = 0.5;
                if (e.period >= 50000) {
                    switch (e.type) {
                    case kSquare:
                        wave = phase < 0.5 ? 1 : 0;
                        break;
                    case kSine:
                        wave = 0.5 + 0.5 * std::sin(phase * 6.283185307179586);
                        break;
                    case kTriangle:
                        wave = 1 - std::abs(phase * 2 - 1);
                        break;
                    case kSawUp:
                        wave = phase;
                        break;
                    case kSawDown:
                        wave = 1 - phase;
                        break;
                    }
                }
                value = std::abs(double(e.offset) / 10000.0 + value * wave);
            }
            value = std::clamp(value * e.gain / 10000.0, 0.0, 1.0);
            level = std::max(level, uint16_t(value * 65535));
        }
        if (level == g_effect_level)
            return;
        g_effect_level = level;
    }
    host_pad_rumble(level, level);
}
