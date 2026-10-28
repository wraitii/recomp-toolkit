struct DiEffect {
    uint32_t device = 0, type = 0, id = 0, generation = 0, valid = 0, revision = 0;
    HostHapticEffect params{}, loaded{};
    uint64_t started = 0, ends = 0;
    bool native = false, downloaded = false, dirty = false, playing = false, busy = false;
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
constexpr uint32_t kEffectAll = 0x3ffu;
constexpr uint32_t kEffectStart = 0x20000000u;
constexpr uint32_t kEffectNoRestart = 0x40000000u;
constexpr uint32_t kEffectNoDownload = 0x80000000u;
constexpr uint32_t kEffectPlaying = 0x80040208u;
constexpr uint32_t kEffectIncomplete = 0x80040206u;
constexpr uint32_t kEffectBusy = 0x800700aau;
constexpr uint32_t kNotExclusive = 0x80040205u;
constexpr uint32_t kNotDownloaded = 0x80040203u;
const uint8_t kEffectGuidSuffix[12] = {0x33, 0x8e, 0xd0, 0x11, 0x9a, 0xd0,
                                       0x00, 0xa0, 0xc9, 0xa0, 0x6e, 0x35};
const char *const kEffectNames[] = {"Constant force", "Ramp force",  "Square",        "Sine",
                                    "Triangle",       "Sawtooth up", "Sawtooth down", "Spring",
                                    "Damper",         "Inertia",     "Friction"};
std::mutex g_effect_mutex;
std::map<uint32_t, DiEffect> g_effects;
std::set<uint32_t> g_muted_devices;
void feedback_tick();
uint16_t g_effect_level = 0;
std::atomic<uint32_t> g_effect_gain{10000};

uint32_t effect_capabilities(uint32_t *generation = nullptr) {
    HostHapticRequest r{};
    r.operation = HOST_HAPTIC_CAPS;
    int status = host_haptic_request(&r);
    if (status == -3) {
        if (generation)
            *generation = 0;
        return 0;
    }
    if (status > 0 && r.features) {
        if (generation)
            *generation = r.result_generation;
        return r.features;
    }
    if (generation)
        *generation = 0;
    return status > 0 && (r.flags & 1) ? 0x7du : 0;
}

uint32_t effect_set_property(uint32_t prop, uint32_t value) {
    if ((prop == 7 && value > 10000) || (prop == 9 && value > 1))
        return DIERR_INVALIDPARAM;
    uint32_t generation = 0;
    uint32_t caps = effect_capabilities(&generation);
    if (!caps)
        return DIERR_INPUTLOST;
    if (!generation) {
        if (prop == 7) {
            g_effect_gain.store(value);
            return DI_OK;
        }
        if (!value)
            return DI_OK;
        log_once("dinput.autocenter", "dinput: force feedback autocenter is unsupported");
        return DIERR_UNSUPPORTED;
    }
    HostHapticRequest r{};
    r.generation = generation;
    r.operation = prop == 7 ? HOST_HAPTIC_GAIN : HOST_HAPTIC_AUTOCENTER;
    r.value = prop == 7 ? value : value * 10000;
    if (host_haptic_request(&r) > 0)
        return DI_OK;
    log_once("dinput.ffproperty", "dinput: SDL force feedback property is unsupported");
    return DIERR_UNSUPPORTED;
}

bool effect_parameters(DiEffect &effect, ComObj *device, uint32_t at, uint32_t flags) {
    if (!at || !gm_valid(at, 52) || rd32(at) < 52)
        return false;
    HostHapticEffect &p = effect.params;
    uint32_t format = rd32(at + 4);
    if (flags & 0x001)
        p.duration = rd32(at + 8);
    if (flags & 0x002)
        p.sample_period = rd32(at + 12);
    if (flags & 0x004) {
        p.gain = rd32(at + 16);
        if (p.gain > 10000)
            return false;
    }
    if (flags & 0x008) {
        uint32_t button = rd32(at + 20);
        if (button == 0xffffffffu)
            p.trigger_button = 0;
        else {
            uint32_t how = (format & 3) == 2 ? DIPH_BYOFFSET : (format & 3) == 1 ? DIPH_BYID : 0;
            int index = how ? joy_haptic_button(device, button, how) : -1;
            if (index < 0)
                return false;
            p.trigger_button = uint32_t(index + 1);
        }
    }
    if (flags & 0x010)
        p.trigger_interval = rd32(at + 24);
    if (flags & 0x020) {
        uint32_t count = rd32(at + 28), axes = rd32(at + 32);
        if (!count || count > 3 || !gm_valid(axes, count * 4))
            return false;
        uint32_t how = (format & 3) == 2 ? DIPH_BYOFFSET : (format & 3) == 1 ? DIPH_BYID : 0;
        if (!how)
            return false;
        for (uint32_t i = 0; i < count; ++i) {
            int axis = joy_haptic_axis(device, rd32(axes + i * 4), how);
            if (axis < 0)
                return false;
            p.axis[i] = uint32_t(axis);
        }
        p.axes = count;
    }
    if (flags & 0x040) {
        uint32_t count = rd32(at + 28), direction = rd32(at + 36);
        uint32_t mode = format & 0x70;
        if (!count || count > 3 || (p.axes && p.axes != count) ||
            (mode != 0x10 && mode != 0x20 && mode != 0x40))
            return false;
        uint32_t dimensions = mode == 0x10 ? count : mode == 0x20 ? 1 : std::max(1u, count - 1);
        if (!gm_valid(direction, dimensions * 4))
            return false;
        p.coordinates = mode == 0x10 ? 1 : mode == 0x20 ? 2 : 3;
        for (uint32_t i = 0; i < dimensions; ++i)
            p.direction[i] = int32_t(rd32(direction + i * 4));
    }
    if (flags & 0x080) {
        uint32_t env = rd32(at + 40);
        p.envelope = env != 0;
        if (env) {
            if (!gm_valid(env, 20) || rd32(env) < 20)
                return false;
            p.attack_level = rd32(env + 4);
            p.attack_time = rd32(env + 8);
            p.fade_level = rd32(env + 12);
            p.fade_time = rd32(env + 16);
            if (p.attack_level > 10000 || p.fade_level > 10000)
                return false;
        }
    }
    if (flags & 0x100) {
        uint32_t size = rd32(at + 44), data = rd32(at + 48);
        if (effect.type == kConstantForce) {
            if (size < 4 || !gm_valid(data, 4))
                return false;
            p.magnitude = int32_t(rd32(data));
        } else if (effect.type == kRampForce) {
            if (size < 8 || !gm_valid(data, 8))
                return false;
            p.ramp_start = int32_t(rd32(data));
            p.ramp_end = int32_t(rd32(data + 4));
        } else if (effect.type <= kSawDown) {
            if (size < 16 || !gm_valid(data, 16))
                return false;
            p.magnitude = int32_t(rd32(data));
            p.offset = int32_t(rd32(data + 4));
            p.phase = rd32(data + 8);
            p.period = rd32(data + 12);
        } else {
            if (!p.axes || size < p.axes * 24 || !gm_valid(data, p.axes * 24))
                return false;
            for (uint32_t i = 0; i < p.axes; ++i) {
                uint32_t a = data + i * 24;
                auto &v = p.condition[i];
                v.offset = int32_t(rd32(a));
                v.positive_coefficient = int32_t(rd32(a + 4));
                v.negative_coefficient = int32_t(rd32(a + 8));
                v.positive_saturation = rd32(a + 12);
                v.negative_saturation = rd32(a + 16);
                v.deadband = rd32(a + 20);
            }
        }
    }
    if (flags & 0x200) {
        if (rd32(at) < 56 || !gm_valid(at, 56))
            return false;
        p.start_delay = rd32(at + 52);
    }
    effect.valid |= flags & kEffectAll;
    return true;
}

bool effect_complete(const DiEffect &e) {
    constexpr uint32_t required = 0x161;
    return (e.valid & required) == required;
}
bool effect_exclusive(const DiEffect &e) {
    ComObj *device = com_get(e.device);
    return device && device->acquired && (device->di_coop & DISCL_EXCLUSIVE);
}
int effect_host(DiEffect &e, uint32_t operation, uint32_t value = 0) {
    HostHapticRequest r{};
    r.operation = operation;
    r.id = e.id;
    r.generation = e.generation;
    r.value = value;
    r.effect = operation == HOST_HAPTIC_START ? e.loaded : e.params;
    int result = host_haptic_request(&r);
    if (result > 0 && operation == HOST_HAPTIC_CREATE) {
        e.id = r.result_id;
        e.generation = r.result_generation;
        e.downloaded = true;
    }
    if (result > 0 && (operation == HOST_HAPTIC_CREATE || operation == HOST_HAPTIC_UPDATE)) {
        e.dirty = false;
        e.loaded = e.params;
    }
    return result;
}

uint32_t effect_result(int result) {
    return result > 0     ? DI_OK
           : result == -1 ? DIERR_UNSUPPORTED
           : result == -2 ? E_FAIL
                          : DIERR_INPUTLOST;
}
bool effect_take(uint32_t id, DiEffect &copy) {
    std::lock_guard<std::mutex> lock(g_effect_mutex);
    auto it = g_effects.find(id);
    if (it == g_effects.end() || it->second.busy)
        return false;
    it->second.busy = true;
    copy = it->second;
    return true;
}
void effect_put(uint32_t id, DiEffect copy) {
    bool stale = false, destroy = false;
    {
        std::lock_guard<std::mutex> lock(g_effect_mutex);
        auto it = g_effects.find(id);
        if (it == g_effects.end()) {
            stale = destroy = true;
        } else if (it->second.revision != copy.revision) {
            stale = true;
            destroy = !it->second.downloaded;
        } else {
            it->second = copy;
            it->second.busy = false;
        }
    }
    if (!stale)
        return;
    if (copy.native && copy.downloaded)
        effect_host(copy, destroy ? HOST_HAPTIC_DESTROY : HOST_HAPTIC_STOP);
    std::lock_guard<std::mutex> lock(g_effect_mutex);
    auto it = g_effects.find(id);
    if (it != g_effects.end())
        it->second.busy = false;
}
void effect_stop_device(uint32_t device) {
    std::vector<DiEffect> stopped;
    {
        std::lock_guard<std::mutex> lock(g_effect_mutex);
        for (auto &entry : g_effects)
            if (entry.second.device == device) {
                entry.second.playing = false;
                ++entry.second.revision;
                if (entry.second.native && entry.second.downloaded)
                    stopped.push_back(entry.second);
            }
    }
    for (auto &e : stopped)
        effect_host(e, HOST_HAPTIC_STOP);
}
bool effect_unload_device(uint32_t device) {
    std::vector<DiEffect> old;
    {
        std::lock_guard<std::mutex> lock(g_effect_mutex);
        for (auto &entry : g_effects)
            if (entry.second.device == device) {
                if (entry.second.native && entry.second.downloaded)
                    old.push_back(entry.second);
                entry.second.downloaded = false;
                entry.second.playing = false;
                ++entry.second.revision;
            }
    }
    bool success = true;
    for (auto &e : old)
        if (effect_host(e, HOST_HAPTIC_DESTROY) <= 0)
            success = false;
    return success;
}
void effect_reset() {
    std::vector<DiEffect> old;
    {
        std::lock_guard<std::mutex> lock(g_effect_mutex);
        for (const auto &entry : g_effects)
            if (entry.second.native && entry.second.downloaded)
                old.push_back(entry.second);
        g_effects.clear();
        g_muted_devices.clear();
        g_effect_level = 0;
        g_effect_gain.store(10000);
    }
    for (auto &e : old)
        effect_host(e, HOST_HAPTIC_DESTROY);
}
void effect_forget_device(uint32_t device) {
    {
        std::lock_guard<std::mutex> lock(g_effect_mutex);
        g_muted_devices.erase(device);
    }
    feedback_tick();
}
void effect_set_actuators(uint32_t device, bool enabled) {
    {
        std::lock_guard<std::mutex> lock(g_effect_mutex);
        if (enabled)
            g_muted_devices.erase(device);
        else
            g_muted_devices.insert(device);
    }
    feedback_tick();
}
void effect_destroy(ComObj *effect) {
    DiEffect e;
    {
        std::lock_guard<std::mutex> lock(g_effect_mutex);
        auto it = g_effects.find(effect->id);
        if (it == g_effects.end())
            return;
        e = it->second;
        g_effects.erase(it);
    }
    if (e.native && e.downloaded)
        effect_host(e, HOST_HAPTIC_DESTROY);
    if (ComObj *device = com_get(e.device))
        com_release(device);
}

void Effect_SetParameters(X86 *c) {
    ComObj *obj = com_this(arg(c, 0), IF_DINPUTEFFECT);
    uint32_t id = obj ? obj->id : 0;
    DiEffect e;
    if (!obj || !effect_take(id, e)) {
        com_ret(c, obj ? kEffectBusy : DIERR_INVALIDPARAM);
        return;
    }
    DiEffect next = e;
    uint32_t flags = arg(c, 2);
    ComObj *device = com_get(e.device);
    if (!device || !effect_parameters(next, device, arg(c, 1), flags & kEffectAll)) {
        effect_put(id, e);
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    if (flags & ~(kEffectAll | kEffectNoDownload | kEffectStart | kEffectNoRestart)) {
        effect_put(id, e);
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    if ((flags & kEffectNoDownload) || !effect_exclusive(next) || !effect_complete(next)) {
        next.dirty = true;
        effect_put(id, next);
        uint32_t result = 3;
        if (!(flags & kEffectNoDownload) && (flags & kEffectStart))
            result = !effect_complete(next) ? kEffectIncomplete : kNotExclusive;
        com_ret(c, result);
        return;
    }
    if ((flags & kEffectNoRestart) && !(flags & kEffectStart) && e.playing && e.native) {
        int status = effect_host(e, HOST_HAPTIC_TRACKED_STATUS);
        if (status <= 0) {
            effect_put(id, e);
            com_ret(c, effect_result(status));
            return;
        }
        e.playing = next.playing = status == 2;
        if (e.playing) {
            effect_put(id, e);
            com_ret(c, kEffectPlaying);
            return;
        }
    }
    int result = 1;
    if (e.native)
        result = effect_host(next, e.downloaded ? HOST_HAPTIC_UPDATE : HOST_HAPTIC_CREATE);
    if (result > 0 && !e.native) {
        next.loaded = next.params;
        next.downloaded = true;
        next.dirty = false;
    }
    bool updated = result > 0;
    if (result > 0 && (flags & kEffectStart)) {
        if (e.native)
            result = effect_host(next, HOST_HAPTIC_START, 1);
        if (result > 0) {
            next.started =
                os_monotonic_ns() + (next.native ? 0 : uint64_t(next.loaded.start_delay) * 1000);
            next.ends = next.params.duration == 0xffffffffu
                            ? UINT64_MAX
                            : next.started + uint64_t(next.params.duration) * 1000;
            next.playing = true;
        }
    }
    effect_put(id, updated ? next : e);
    com_ret(c, effect_result(result));
}
void Effect_Start(X86 *c) {
    ComObj *obj = com_this(arg(c, 0), IF_DINPUTEFFECT);
    uint32_t id = obj ? obj->id : 0;
    DiEffect e;
    uint32_t count = arg(c, 1), flags = arg(c, 2);
    if (!obj || !count || !effect_take(id, e)) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    ComObj *device = com_get(e.device);
    if (!device || !device->acquired || !(device->di_coop & DISCL_EXCLUSIVE)) {
        effect_put(id, e);
        com_ret(c, kNotExclusive);
        return;
    }
    if (!effect_complete(e)) {
        effect_put(id, e);
        com_ret(c, kEffectIncomplete);
        return;
    }
    if (flags & ~(0x80000000u | 1u)) {
        effect_put(id, e);
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    if (flags & 1) {
        effect_stop_device(e.device);
        std::lock_guard<std::mutex> lock(g_effect_mutex);
        auto it = g_effects.find(id);
        if (it != g_effects.end())
            e.revision = it->second.revision;
    }
    int result = 1;
    if (!e.downloaded && (flags & 0x80000000u)) {
        effect_put(id, e);
        com_ret(c, kNotDownloaded);
        return;
    }
    if (result > 0 && !(flags & 0x80000000u) && (!e.downloaded || e.dirty)) {
        if (e.native)
            result = effect_host(e, e.downloaded ? HOST_HAPTIC_UPDATE : HOST_HAPTIC_CREATE);
        else {
            e.loaded = e.params;
            e.downloaded = true;
            e.dirty = false;
        }
    }
    if (result > 0 && e.native)
        result = effect_host(e, HOST_HAPTIC_START, count);
    if (result > 0) {
        e.started = os_monotonic_ns() + (e.native ? 0 : uint64_t(e.loaded.start_delay) * 1000);
        uint64_t span = uint64_t(e.loaded.duration) * 1000;
        e.ends = e.loaded.duration == 0xffffffffu || count == 0xffffffffu ||
                         span > (UINT64_MAX - e.started) / count
                     ? UINT64_MAX
                     : e.started + span * count;
        e.playing = true;
    }
    effect_put(id, e);
    com_ret(c, effect_result(result));
}
void Effect_Stop(X86 *c) {
    ComObj *obj = com_this(arg(c, 0), IF_DINPUTEFFECT);
    uint32_t id = obj ? obj->id : 0;
    DiEffect e;
    if (!obj || !effect_take(id, e)) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    int result = e.native && e.downloaded ? effect_host(e, HOST_HAPTIC_STOP) : 1;
    e.playing = false;
    effect_put(id, e);
    com_ret(c, effect_result(result));
}
void Effect_GetEffectStatus(X86 *c) {
    ComObj *obj = com_this(arg(c, 0), IF_DINPUTEFFECT);
    uint32_t out = arg(c, 1);
    uint32_t id = obj ? obj->id : 0;
    DiEffect e;
    if (!obj || !out || !gm_valid(out, 4) || !effect_take(id, e)) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    if (e.native && e.downloaded) {
        int result = effect_host(e, HOST_HAPTIC_STATUS);
        if (result <= 0) {
            effect_put(id, e);
            log_once("dinput.effect.status", "dinput: SDL haptic status is unsupported");
            com_ret(c, DIERR_UNSUPPORTED);
            return;
        }
        e.playing = result == 2;
    } else if (e.playing && os_monotonic_ns() >= e.ends)
        e.playing = false;
    wr32(out, e.playing ? 1 : 0);
    effect_put(id, e);
    com_ret(c, DI_OK);
}
DX_STUB(Effect_Initialize, DIERR_UNSUPPORTED)
void Effect_GetEffectGuid(X86 *c) {
    ComObj *obj = com_this(arg(c, 0), IF_DINPUTEFFECT);
    uint32_t out = arg(c, 1);
    std::lock_guard<std::mutex> lock(g_effect_mutex);
    auto it = obj ? g_effects.find(obj->id) : g_effects.end();
    if (it == g_effects.end() || !gm_valid(out, 16)) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    wr32(out, it->second.type);
    memcpy(gm_ptr(out + 4), kEffectGuidSuffix, 12);
    com_ret(c, DI_OK);
}
DX_STUB(Effect_GetParameters, DIERR_UNSUPPORTED)
void Effect_Download(X86 *c) {
    ComObj *obj = com_this(arg(c, 0), IF_DINPUTEFFECT);
    uint32_t id = obj ? obj->id : 0;
    DiEffect e;
    if (!obj || !effect_take(id, e)) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    if (!effect_complete(e) || !effect_exclusive(e)) {
        effect_put(id, e);
        com_ret(c, !effect_complete(e) ? kEffectIncomplete : kNotExclusive);
        return;
    }
    int result =
        e.native ? effect_host(e, e.downloaded ? HOST_HAPTIC_UPDATE : HOST_HAPTIC_CREATE) : 1;
    if (result > 0 && !e.native) {
        e.loaded = e.params;
        e.downloaded = true;
        e.dirty = false;
    }
    effect_put(id, e);
    com_ret(c, effect_result(result));
}
void Effect_Unload(X86 *c) {
    ComObj *obj = com_this(arg(c, 0), IF_DINPUTEFFECT);
    uint32_t id = obj ? obj->id : 0;
    DiEffect e;
    if (!obj || !effect_take(id, e)) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    int result = e.native && e.downloaded ? effect_host(e, HOST_HAPTIC_DESTROY) : 1;
    e.downloaded = e.playing = false;
    effect_put(id, e);
    com_ret(c, effect_result(result));
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
struct EffectDeviceRef {
    ComObj *device;
    ~EffectDeviceRef() {
        if (device)
            com_release(device);
    }
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
    com_addref(device);
    EffectDeviceRef device_ref{device};
    uint32_t type = rd32(guid), generation = 0;
    uint32_t caps = effect_capabilities(&generation);
    if (memcmp(gm_ptr(guid + 4), kEffectGuidSuffix, 12) || type < kConstantForce ||
        type > kConstantForce + 10 || !(caps & (1u << (type - kConstantForce)))) {
        char key[80];
        snprintf(key, sizeof(key), "dinput.effect.%08x.%08x.%08x.%08x", type, rd32(guid + 4),
                 rd32(guid + 8), rd32(guid + 12));
        log_once(key, "dinput: unsupported force feedback effect %08x", type);
        com_ret(c, DIERR_UNSUPPORTED);
        return;
    }
    DiEffect e;
    e.device = device->id;
    e.type = type;
    e.params.type = type - kConstantForce;
    e.params.gain = 10000;
    e.params.trigger_button = 0;
    e.generation = generation;
    e.native = generation != 0;
    if (params &&
        !effect_parameters(e, device, params,
                           gm_valid(params, 56) && rd32(params) >= 56 ? kEffectAll : 0x1ff)) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    if (params && effect_exclusive(e) && e.native) {
        int result = effect_host(e, HOST_HAPTIC_CREATE);
        if (result <= 0) {
            com_ret(c, effect_result(result));
            return;
        }
    }
    if (params && effect_exclusive(e) && !e.native) {
        e.loaded = e.params;
        e.downloaded = true;
    }
    ComObj *obj = com_new(K_DINPUTEFFECT);
    uint32_t view = com_view(obj, IF_DINPUTEFFECT);
    if (!view) {
        if (e.native && e.downloaded)
            effect_host(e, HOST_HAPTIC_DESTROY);
        com_release(obj);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    {
        std::lock_guard<std::mutex> lock(g_effect_mutex);
        g_effects[obj->id] = e;
    }
    device_ref.device = nullptr;
    wr32(out, view);
    com_ret(c, DI_OK);
}
uint32_t effect_class(uint32_t index) {
    return index == 0 ? 1 : index == 1 ? 2 : index <= 6 ? 3 : 4;
}
void write_effect_info(uint32_t at, uint32_t index, bool wide, bool native) {
    uint32_t size = wide ? 552 : 292;
    gm_zero(at, size);
    wr32(at, size);
    wr32(at + 4, kConstantForce + index);
    memcpy(gm_ptr(at + 8), kEffectGuidSuffix, 12);
    wr32(at + 20, effect_class(index));
    wr32(at + 24, native ? (index >= 7 ? 0x37d : 0x3fd) : 0x105);
    wr32(at + 28, 0);
    if (wide)
        di_put_wide(at + 32, kEffectNames[index], 260);
    else
        gm_put_str(at + 32, kEffectNames[index], 260);
}
void Device_EnumEffects(X86 *c) {
    ComObj *device = this_device(c);
    uint32_t callback = arg(c, 1), ref = arg(c, 2), filter = arg(c, 3) & 0xff;
    if (!device || !is_joystick(device) || !callback) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    bool wide = device->di_wide;
    uint32_t generation = 0;
    uint32_t caps = effect_capabilities(&generation);
    for (uint32_t i = 0; i < std::size(kEffectNames); ++i) {
        if (!(caps & (1u << i)) || (filter && filter != effect_class(i)))
            continue;
        uint32_t size = wide ? 552 : 292;
        uint32_t at = scratch(size);
        if (!at) {
            com_ret(c, E_OUTOFMEMORY);
            return;
        }
        write_effect_info(at, i, wide, generation != 0);
        if (guest_call(c, callback, at, ref) != 1)
            break;
    }
    com_ret(c, DI_OK);
}
void Device_GetEffectInfo(X86 *c) {
    ComObj *device = this_device(c);
    uint32_t out = arg(c, 1), guid = arg(c, 2);
    if (!device || !is_joystick(device) || !gm_valid(guid, 16) || !gm_valid(out, 4)) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    bool wide = device->di_wide;
    uint32_t size = wide ? 552 : 292;
    uint32_t type = rd32(guid);
    if (rd32(out) != size || !gm_valid(out, size)) {
        com_ret(c, DIERR_INVALIDPARAM);
        return;
    }
    uint32_t generation = 0, caps = effect_capabilities(&generation);
    if (memcmp(gm_ptr(guid + 4), kEffectGuidSuffix, 12) || type < kConstantForce ||
        type > kConstantForce + 10 || !(caps & (1u << (type - kConstantForce)))) {
        com_ret(c, DIERR_UNSUPPORTED);
        return;
    }
    write_effect_info(out, type - kConstantForce, wide, generation != 0);
    com_ret(c, DI_OK);
}
void Device_SendForceFeedbackCommand(X86 *c) {
    ComObj *device = this_device(c);
    if (!device || !is_joystick(device)) {
        com_ret(c, DIERR_UNSUPPORTED);
        return;
    }
    uint32_t device_id = device->id;
    uint32_t generation = 0;
    uint32_t caps = effect_capabilities(&generation);
    uint32_t op = arg(c, 1);
    if (!caps) {
        com_ret(c, DIERR_INPUTLOST);
        return;
    }
    if (op == 1 || op == 0x10 || op == 0x20) {
        ComObj *current = com_get(device_id);
        if (!current || !current->acquired || !(current->di_coop & DISCL_EXCLUSIVE)) {
            com_ret(c, kNotExclusive);
            return;
        }
        if (generation) {
            log_once("dinput.ffactuators", "dinput: SDL native actuator control is unsupported");
            com_ret(c, DIERR_UNSUPPORTED);
            return;
        }
        if (op == 1 && !effect_unload_device(device_id)) {
            com_ret(c, DIERR_INPUTLOST);
            return;
        }
        effect_set_actuators(device_id, op == 0x10);
        com_ret(c, DI_OK);
        return;
    }
    if (op == 2)
        effect_stop_device(device_id);
    if (!generation && op == 2) {
        com_ret(c, DI_OK);
        return;
    }
    HostHapticRequest r{};
    r.generation = generation;
    if (op == 2)
        r.operation = HOST_HAPTIC_STOP_ALL;
    else if (op == 4)
        r.operation = HOST_HAPTIC_PAUSE;
    else if (op == 8)
        r.operation = HOST_HAPTIC_RESUME;
    else {
        log_once("dinput.ffcommand", "dinput: unsupported force feedback command %08x", op);
        com_ret(c, DIERR_UNSUPPORTED);
        return;
    }
    com_ret(c, effect_result(host_haptic_request(&r)));
}

void feedback_tick() {
    uint16_t level = 0;
    bool enabled = host_pad_dinput_rumble_enabled() != 0;
    {
        std::lock_guard<std::mutex> lock(g_effect_mutex);
        const uint64_t now = os_monotonic_ns();
        for (auto &entry : g_effects) {
            DiEffect &e = entry.second;
            if (e.native || !e.playing)
                continue;
            if (now >= e.ends) {
                e.playing = false;
                continue;
            }
            if (now < e.started || !enabled || g_muted_devices.count(e.device))
                continue;
            // DIVERGENCE(original): directional wheel forces are approximated by gamepad motors.
            double value = std::abs(double(e.loaded.magnitude)) / 10000.0;
            if (e.type != kConstantForce) {
                double phase =
                    e.loaded.period
                        ? double((now - e.started) / 1000 % e.loaded.period) / e.loaded.period
                        : 0;
                double wave = 0.5;
                if (e.loaded.period >= 50000) {
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
                value = std::abs(double(e.loaded.offset) / 10000.0 + value * wave);
            }
            value =
                std::clamp(value * e.loaded.gain * g_effect_gain.load() / 100000000.0, 0.0, 1.0);
            level = std::max(level, uint16_t(value * 65535));
        }
        if (level == g_effect_level)
            return;
        g_effect_level = level;
        host_pad_dinput_rumble(level);
    }
}
