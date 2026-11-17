// mss32.cpp - Miles Sound System 6 (mss32.dll) as a game imports it:
// stdcall exports whose decorated names carry their arity (_AIL_name@bytes).
// Samples play PCM WAVE images and streams decode MP3 through shared host
// audio channels. The 3D provider API reports "no providers" so a game can
// use its plain 2D path. Every entry preserves the guest's stdcall stack.
#include "com.h"
#include "dx.h"
#include "audio3d.h"
#include "host_api.h"
#include "riff.h"
#include "mp3_source.h"
#include "wav_source.h"
#include "../runtime/imports.h"
#include "../runtime/memory.h"
#include "../runtime/win32.h"
#include "../platform/os.h"

#include <algorithm>
#include <cmath>
#include <cstring>
#include <cstdlib>
#include <vector>
#include <iterator>
#include <map>
#include <mutex>
#include <condition_variable>
#include <chrono>
#include <utility>

namespace {

const uint32_t SMP_FREE = 1, SMP_DONE = 2, SMP_PLAYING = 4;
constexpr uint32_t STREAM_BUFFER_MS = 1000;

int32_t miles_volume_mb(int32_t v) {
    v = std::clamp(v, 0, 127);
    return v == 0 ? -10000
                  : std::clamp((int32_t)std::lround(2000.0 * std::log10(v / 127.0)), -10000, 0);
}

int32_t miles_volume_mb(float v) {
    v = std::clamp(v, 0.0f, 1.0f);
    return v == 0.0f ? -10000 : std::clamp((int32_t)std::lround(2000.0 * std::log10(v)), -10000, 0);
}

int32_t miles_pan_mb(float p) {
    p = std::clamp(p, 0.0f, 1.0f);
    return std::clamp((int32_t)std::lround((p - 0.5f) * 20000.0f), -10000, 10000);
}

struct Driver {
    uint32_t rate = 22050, bits = 16, channels = 2;
    int master = 127;
    ComObj *ds_object = nullptr;
    ComObj *primary_buffer = nullptr;
    uint32_t reverb[3]{};
    uint32_t reverb_dry = 0x3f800000, reverb_wet = 0;
    uint32_t room_type = 0;
    float doppler = 1.0f;
};
std::map<uint32_t, Driver> g_drivers;
uint32_t g_next_driver = 0x10000;
int32_t g_preferences[64]{};
uint32_t g_malloc_cb = 0, g_free_cb = 0;

void driver_apply_mix(uint32_t driver);

float bits_to_float(uint32_t bits) {
    float v;
    memcpy(&v, &bits, 4);
    return v;
}

int driver_volume(uint32_t driver, float volume) {
    auto it = g_drivers.find(driver);
    if (it == g_drivers.end())
        return miles_volume_mb(volume);
    return std::clamp(miles_volume_mb(it->second.master / 127.0f) + miles_volume_mb(volume), -10000,
                      0);
}

float driver_dry(uint32_t driver) {
    auto it = g_drivers.find(driver);
    return it == g_drivers.end() ? 1.0f : bits_to_float(it->second.reverb_dry);
}

float driver_wet(uint32_t driver) {
    auto it = g_drivers.find(driver);
    return it == g_drivers.end() ? 0.0f : bits_to_float(it->second.reverb_wet);
}

struct Sample {
    uint32_t handle = 0, driver = 0;
    std::vector<int16_t> decoded;
    bool alive = false;
    int32_t channel = -1;
    RiffWave wave{};
    float volume01 = 1.0f, pan01 = 0.5f;
    uint32_t reverb_dry = 0x3f800000, reverb_wet = 0;
    uint32_t loops = 1, remaining = 0;
    bool playing = false;
    uint32_t eos_cb = 0; // AIL_register_EOS_callback for a 2D sample
    bool eos_fired = false;
    int32_t stream = -1;

    // A Miles 3D sample plays through the same host-channel machinery as a 2D
    // one. Where it is and how far away decide the pan and the distance gain;
    // rate overrides the file's own sample rate when non-zero.
    bool is_3d = false;
    float pos3d[3] = {0.0f, 0.0f, 0.0f};
    float front3d[3] = {0.0f, 0.0f, 1.0f};
    float up3d[3] = {0.0f, 1.0f, 0.0f};
    float vel3d[3] = {0.0f, 0.0f, 0.0f};
    float cone_inner = 0.0f, cone_outer = 0.0f, cone_outer_volume = 1.0f;
    bool cone_enabled = false;
    float min_d = 1.0f, max_d = 1.0e9f;
    uint32_t rate = 0;
    float obstruction = 0.0f, occlusion = 0.0f;
    uint32_t user[8] = {};
};
std::vector<Sample> g_samples(64);
std::mutex g_stream_mutex;
std::condition_variable g_stream_condition;
OsThread *g_stream_thread = nullptr;
bool g_stream_worker_stop = false;
bool g_stream_command = false;
bool g_stream_host_shutdown = false;

uint32_t stream_status(uint32_t handle);
void stream_close_driver(uint32_t driver);
void stream_stop_alias(int32_t index);
bool stream_worker_start();

// The single digital driver every sample belongs to. A sample handle does not
// name a driver, so master volume comes from the most recently opened one.
uint32_t g_last_digital_driver = 0;

// Miles' 3D listener. RT3 sets an identity orientation and the origin (its
// sound code pre-transforms world positions into listener space), so these are
// the defaults a source is placed against until the title says otherwise.
float g_listener_pos3d[3] = {0.0f, 0.0f, 0.0f};
float g_listener_front3d[3] = {0.0f, 0.0f, 1.0f};
float g_listener_top3d[3] = {0.0f, 1.0f, 0.0f};
float g_rolloff3d = 1.0f;

// The listener handle the shim hands out; distinct from sample handles so
// AIL_set_3D_position/orientation can tell the two apart.
const uint32_t kListener3DHandle = 0x00030001u;

// Miles timers. RT3 uses one to fade the music stream: AIL_register_timer
// names a callback, then a frequency and user word are set and the timer is
// started. Miles fired these on its own thread; the shim runs them on the
// guest frame seam, which is the thread the callback already assumes because
// it touches the guest heap and the music stream.
struct AilTimer {
    bool alive = false;
    bool running = false;
    uint32_t handle = 0;
    uint32_t callback = 0;
    uint32_t user = 0;
    uint32_t frequency = 0;
    uint64_t next_ns = 0;
};
std::vector<AilTimer> g_ail_timers(16);

Sample *sample_for(uint32_t handle) {
    if (!handle || handle > g_samples.size())
        return nullptr;
    Sample &s = g_samples[handle - 1];
    return s.alive ? &s : nullptr;
}

// The sample borrows its WAVE image. The host copies PCM on submission;
// Miles keeps the shared channel reserved until init or handle release.
void sample_stop(Sample &s) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    stream_stop_alias(s.stream);
    if (s.channel >= 0)
        host_audio_stop(s.channel);
    s.playing = false;
    s.remaining = 0;
}

void sample_reset(Sample &s) {
    sample_stop(s);
    if (s.stream < 0)
        dx_free_audio_channel(s.channel);
    uint32_t handle = s.handle, driver = s.driver;
    s = Sample{};
    s.handle = handle;
    s.driver = driver;
    s.alive = true;
}

// Inverse-distance attenuation against the listener, matching the
// DirectSound3D model in dsound.cpp's calc_3d: a source inside MinDistance is
// untouched, beyond it falls off and is clamped at MaxDistance.
float sample3d_distance_gain(const Sample &s) {
    audio3d::V3 dir = {s.pos3d[0] - g_listener_pos3d[0], s.pos3d[1] - g_listener_pos3d[1],
                       s.pos3d[2] - g_listener_pos3d[2]};
    float dist = audio3d::v3_len(dir);
    if (dist > s.max_d)
        dist = s.max_d;
    if (dist < s.min_d)
        dist = s.min_d;
    float adjusted = s.min_d + (dist - s.min_d) * g_rolloff3d;
    return (s.min_d > 0.0f && adjusted > 0.0f) ? s.min_d / adjusted : 1.0f;
}

float sample3d_cone_gain(const Sample &s) {
    if (!s.cone_enabled)
        return 1.0f;
    audio3d::V3 dir = {s.pos3d[0] - g_listener_pos3d[0], s.pos3d[1] - g_listener_pos3d[1],
                       s.pos3d[2] - g_listener_pos3d[2]};
    float dist = audio3d::v3_len(dir);
    if (dist < 1.0e-4f) {
        dir = {1.0f, 0.0f, 0.0f};
    } else {
        float inverse = 1.0f / dist;
        dir.x *= inverse;
        dir.y *= inverse;
        dir.z *= inverse;
    }
    float dot = -(s.front3d[0] * dir.x + s.front3d[1] * dir.y + s.front3d[2] * dir.z);
    float angle;
    if (dot > 0.9999f)
        angle = 0.0f;
    else if (dot < -0.9999f)
        angle = audio3d::PI_F_;
    else
        angle = (float)acos((double)dot);
    if (angle > s.cone_outer)
        return s.cone_outer_volume;
    if (angle > s.cone_inner && s.cone_outer - s.cone_inner > 1.0e-4f)
        return s.cone_outer_volume + (s.cone_outer - angle) / (s.cone_outer - s.cone_inner) *
                                         (1.0f - s.cone_outer_volume);
    return 1.0f;
}

void sample3d_normalize_or_keep(float *v) {
    float len = sqrtf(v[0] * v[0] + v[1] * v[1] + v[2] * v[2]);
    if (len > 1.0e-4f) {
        float inverse = 1.0f / len;
        v[0] *= inverse;
        v[1] *= inverse;
        v[2] *= inverse;
    }
}

// The hundredths-of-a-dB pan for a listener-relative source, using the same
// axis convention and speaker mix as DirectSound3D (front +z, top +y).
int32_t sample3d_pan_mb(const Sample &s) {
    audio3d::V3 dir = {s.pos3d[0] - g_listener_pos3d[0], s.pos3d[1] - g_listener_pos3d[1],
                       s.pos3d[2] - g_listener_pos3d[2]};
    float dist = audio3d::v3_len(dir);
    if (dist == 0.0f)
        return 0;
    audio3d::V3 front = {g_listener_front3d[0], g_listener_front3d[1], g_listener_front3d[2]};
    audio3d::V3 top = {g_listener_top3d[0], g_listener_top3d[1], g_listener_top3d[2]};
    audio3d::V3 left = audio3d::v3_cross(front, top);
    float angle = audio3d::v3_angle(left, dir);
    if (audio3d::v3_angle(front, dir) > audio3d::PI_F_ / 2.0f)
        angle = -angle;
    angle -= audio3d::PI_F_ / 2.0f;
    if (angle < -audio3d::PI_F_)
        angle += 2.0f * audio3d::PI_F_;
    float lg = 1.0f, rg = 1.0f;
    audio3d::stereo_gains(angle, &lg, &rg);
    int32_t pan;
    if (lg > 0.0f && rg > 0.0f)
        pan = (int32_t)std::lround(2000.0f * std::log10(rg / lg));
    else if (rg > lg)
        pan = 10000;
    else
        pan = -10000;
    return std::clamp(pan, -10000, 10000);
}

int sample_volume(const Sample &s) {
    float gain3d = s.is_3d ? sample3d_distance_gain(s) * sample3d_cone_gain(s) : 1.0f;
    return driver_volume(s.driver, s.volume01 * gain3d);
}

// Push a sample's current volume and pan to its host channel. A playing 3D
// source moves, so this is called on every position change as well as start.
void sample_apply_mix(Sample &s) {
    if (s.channel < 0)
        return;
    host_audio_set_volume(s.channel, std::max(-10000, sample_volume(s)));
    host_audio_set_pan(s.channel, s.is_3d ? sample3d_pan_mb(s) : miles_pan_mb(s.pan01));
    host_audio_set_reverb_send(s.channel, s.driver,
                               bits_to_float(s.reverb_dry) * driver_dry(s.driver),
                               bits_to_float(s.reverb_wet) * driver_wet(s.driver));
}

void sample_update_3d_mix() {
    for (auto &s : g_samples)
        if (s.alive && s.is_3d)
            sample_apply_mix(s);
}

void sample_play(Sample &s) {
    HostAudioPlay p{};
    p.channel = s.channel;
    p.pcm = s.decoded.empty() ? (const void *)(g_mem + s.wave.pcm) : s.decoded.data();
    p.bytes = s.wave.pcm_bytes;
    p.sample_rate = (int32_t)(s.rate ? s.rate : s.wave.rate);
    p.channels = s.wave.channels;
    p.bits = s.wave.bits;
    p.loop = s.loops == 0 ? 1 : 0;
    p.volume = std::max(-10000, sample_volume(s));
    p.pan = s.is_3d ? sample3d_pan_mb(s) : miles_pan_mb(s.pan01);
    host_audio_set_reverb_send(s.channel, s.driver,
                               bits_to_float(s.reverb_dry) * driver_dry(s.driver),
                               bits_to_float(s.reverb_wet) * driver_wet(s.driver));
    host_audio_set_frequency(s.channel, s.rate);
    host_audio_play(&p);
    s.playing = true;
}

// The host loop flag is boolean. Finite repeats restart at completion on
// the runtime's frame seam (also observed by status), with n total plays.
// Returns true only when the sample naturally finished, which is when the
// registered EOS callback fires.
bool sample_update(Sample &s) {
    if (!s.playing || host_audio_is_playing(s.channel))
        return false;
    if (s.remaining > 1) {
        --s.remaining;
        sample_play(s);
        return false;
    }
    s.remaining = 0;
    s.playing = false;
    return true;
}

void ail_default_malloc(X86 *c) {
    uint32_t block = heap_alloc(arg(c, 0) + 32);
    if (!block) {
        set_eax(c, 0);
        return;
    }
    uint32_t aligned = (block + 32) & ~31u;
    wr32(aligned - 4, block);
    set_eax(c, aligned);
}

void ail_default_free(X86 *c) {
    uint32_t ptr = arg(c, 0), block = rd32(ptr - 4);
    wr32(ptr - 4, 0);
    heap_free(block);
}

uint32_t ail_malloc_callback() {
    return g_malloc_cb
               ? g_malloc_cb
               : imports_alloc_trampoline("mss32.dll", "MilesDefaultMalloc", ail_default_malloc, 1);
}

uint32_t ail_free_callback() {
    return g_free_cb
               ? g_free_cb
               : imports_alloc_trampoline("mss32.dll", "MilesDefaultFree", ail_default_free, 1);
}

void ail_mem_use_malloc(X86 *c) {
    uint32_t previous = ail_malloc_callback();
    g_malloc_cb = arg(c, 0);
    set_eax(c, previous);
}

void ail_mem_use_free(X86 *c) {
    uint32_t previous = ail_free_callback();
    g_free_cb = arg(c, 0);
    set_eax(c, previous);
}

uint32_t ail_alloc(X86 *c, uint32_t bytes) {
    return guest_call(c, ail_malloc_callback(), bytes);
}

void ail_free(X86 *c, uint32_t ptr) {
    if (ptr)
        guest_call(c, ail_free_callback(), ptr);
}

void ail_mem_alloc_lock(X86 *c) {
    set_eax(c, ail_alloc(c, arg(c, 0)));
}

// DIVERGENCE(original): the kit exposes no audio input device, so
// AIL_open_input fails as Miles does without one and voice capture never starts.
void ail_open_input(X86 *c) {
    log_once("mss32.input", "mss32: AIL_open_input: no audio input device is available");
    set_eax(c, 0);
}

void ail_ms_count(X86 *c) {
    set_eax(c, host_millis());
}

// The shim reports failures through return values only, so the last error is
// always the empty string Miles returns before any failure.
void ail_last_error(X86 *c) {
    static uint32_t text = 0;
    if (!text)
        text = heap_alloc(4, true);
    set_eax(c, text);
}

void ail_file_size(X86 *c) {
    std::string path = win32_host_path_op(gm_str(arg(c, 0)), WIN32_FILE_READ);
    OsStat st{};
    bool ok =
        !path.empty() && os_stat(path.c_str(), &st) == 0 && st.is_regular && st.size <= INT32_MAX;
    set_eax(c, ok ? (uint32_t)st.size : 0xffffffffu);
}

// AILFILETYPE_*: 0 unknown, 1 PCM WAV, 2 IMA ADPCM WAV, 3 other WAV, 4 VOC,
// 5 MIDI, 6 XMIDI, 13 MPEG layer 3, 14 WAV that needs an ASI codec.
void ail_file_type(X86 *c) {
    uint32_t data = arg(c, 0), size = arg(c, 1);
    set_eax(c, 0);
    if (!data || size < 4 || !gm_valid(data, size))
        return;
    const uint8_t *p = g_mem + data;
    WavImage wav{};
    if (size >= 12 && !memcmp(p, "RIFF", 4) && !memcmp(p + 8, "WAVE", 4)) {
        uint32_t type = 3;
        if (riff_parse_wave_host(p, size, &wav))
            type = wav.format == 1 ? 1 : wav.format == 0x11 ? 2 : wav.format == 0x55 ? 14 : 3;
        set_eax(c, type);
    } else if (size >= 19 && !memcmp(p, "Creative Voice File", 19)) {
        set_eax(c, 4);
    } else if (!memcmp(p, "MThd", 4)) {
        set_eax(c, 5);
    } else if (size >= 12 && !memcmp(p, "FORM", 4) && !memcmp(p + 8, "XDIR", 4)) {
        set_eax(c, 6);
    } else if (!memcmp(p, "ID3", 3) || (p[0] == 0xff && (p[1] & 0xe6) == 0xe2)) {
        set_eax(c, 13);
    }
}

void ail_file_read(X86 *c) {
    set_eax(c, 0);
    std::string path = win32_host_path_op(gm_str(arg(c, 0)), WIN32_FILE_READ);
    if (path.empty())
        return;
    int fd = os_fd_open(path.c_str(), OS_O_RDONLY);
    if (fd < 0)
        return;
    OsStat st{};
    if (os_fd_stat(fd, &st) != 0 || !st.is_regular || !st.size || st.size > UINT32_MAX) {
        os_fd_close(fd);
        return;
    }
    uint32_t bytes = (uint32_t)st.size, dest = arg(c, 1);
    bool allocate = dest == 0 || dest == 0xffffffffu;
    if (allocate)
        dest = ail_alloc(c, bytes);
    if (!dest || !gm_valid(dest, bytes)) {
        if (allocate && dest)
            ail_free(c, dest);
        os_fd_close(fd);
        set_eax(c, 0);
        return;
    }
    uint32_t got = 0;
    while (got < bytes) {
        int64_t n = os_fd_read(fd, g_mem + dest + got, bytes - got);
        if (n <= 0)
            break;
        got += (uint32_t)n;
    }
    os_fd_close(fd);
    if (got != bytes) {
        if (allocate)
            ail_free(c, dest);
        set_eax(c, 0);
        return;
    }
    set_eax(c, dest);
}

void ail_mem_free_lock(X86 *c) {
    ail_free(c, arg(c, 0));
    set_eax(c, 0);
}

void ail_allocate_sample_handle(X86 *c) {
    set_eax(c, 0);
    for (uint32_t i = 0; i < g_samples.size(); ++i) {
        Sample &s = g_samples[i];
        if (s.alive)
            continue;
        s = Sample{};
        s.handle = i + 1;
        s.driver = arg(c, 0);
        s.alive = true;
        set_eax(c, s.handle);
        return;
    }
}

void ail_release_sample_handle(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    if (s && s->stream >= 0) {
        imports_unsupported(c);
        return;
    }
    if (s) {
        sample_reset(*s);
        s->alive = false;
    }
    set_eax(c, 0);
}

void ail_init_sample(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    if (s && s->stream >= 0) {
        imports_unsupported(c);
        return;
    }
    if (s)
        sample_reset(*s);
    set_eax(c, 0);
}

// The byte length of an in-memory sound image. Miles entry points take a
// pointer, not a length, so the length comes from the guest heap when the
// image is a heap block (which is how the engine's loader allocates a whole
// file) and otherwise from the RIFF header, which is how an image that lives
// in a fixed buffer is measured.
uint32_t sample_image_bytes(uint32_t image, uint32_t explicit_bytes) {
    if (explicit_bytes)
        return explicit_bytes;
    uint32_t allocated = heap_size(image);
    if (allocated != UINT32_MAX)
        return allocated;
    if (image && gm_valid(image, 12)) {
        uint64_t bytes = (uint64_t)rd32(image + 4) + 8;
        if (bytes <= UINT32_MAX)
            return (uint32_t)bytes;
    }
    return 0;
}

// Load an in-memory sound image into a sample. RT3's effects and music arrive
// as either a RIFF WAVE or a bare MPEG audio file (Miles ships its MP3 codec
// in Data/Miles), so both are accepted: RIFF PCM stays in the guest image, and
// MPEG is decoded once into PCM the sample owns.
bool sample_load_image(Sample &s, uint32_t image, uint32_t bytes) {
    s.wave = RiffWave{};
    s.decoded.clear();
    if (!image || !bytes || !gm_valid(image, bytes)) {
        LOGW("mss32: sample image %08x length %u is not guest memory", image, bytes);
        return false;
    }
    if (riff_parse_wave(image, bytes, &s.wave))
        return s.wave.pcm_bytes != 0;
    std::vector<uint8_t> host(g_mem + image, g_mem + image + bytes);
    WavImage wav{};
    if (riff_parse_wave_host(host.data(), host.size(), &wav)) {
        WavSource source;
        if (!source.open(host, 64 * 1024 * 1024)) {
            LOGW("mss32: sample image %08x: WAVE format %04x bits=%u could not be decoded", image,
                 wav.format, wav.bits);
            return false;
        }
        std::vector<int16_t> frame;
        while (source.decode_next(frame))
            s.decoded.insert(s.decoded.end(), frame.begin(), frame.end());
        if (source.failed()) {
            s.decoded.clear();
            LOGW("mss32: sample image %08x: WAVE decode failed", image);
            return false;
        }
        if (s.decoded.empty()) {
            LOGW("mss32: sample image %08x: WAVE decoded to no PCM", image);
            return false;
        }
        s.wave.pcm_bytes = uint32_t(s.decoded.size() * 2);
        s.wave.rate = source.rate();
        s.wave.channels = (uint16_t)source.channels();
        s.wave.bits = 16;
        return true;
    }
    Mp3Source source;
    if (!source.open(host)) {
        LOGW("mss32: sample image %08x length %u is not a decodable WAVE or MPEG image", image,
             bytes);
        return false;
    }
    std::vector<int16_t> frame;
    while (source.decode_next(frame)) {
        if (s.decoded.size() + frame.size() > 64 * 1024 * 1024) {
            s.decoded.clear();
            LOGW("mss32: sample image %08x: decoded MPEG exceeds 128 MiB", image);
            return false;
        }
        s.decoded.insert(s.decoded.end(), frame.begin(), frame.end());
    }
    s.wave.pcm_bytes = uint32_t(s.decoded.size() * 2);
    s.wave.rate = source.rate();
    s.wave.channels = (uint16_t)source.channels();
    s.wave.bits = 16;
    return s.wave.pcm_bytes != 0;
}

void ail_set_sample_file(X86 *c) {
    set_eax(c, 0);
    Sample *s = sample_for(arg(c, 0));
    if (!s)
        return;
    sample_stop(*s);
    uint32_t image = arg(c, 1), bytes = arg(c, 2);
    if (sample_load_image(*s, image, sample_image_bytes(image, bytes)))
        set_eax(c, 1);
}

// Named samples carry a byte length, allowing packed MP3 effects as well as
// RIFF. This is also the shared decoder behind the file-image entry points.
void ail_set_named_sample_file(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    set_eax(c, 0);
    if (!s)
        return;
    sample_stop(*s);
    s->rate = 0;
    if (sample_load_image(*s, arg(c, 2), arg(c, 3)))
        set_eax(c, 1);
    else {
        std::string name = arg(c, 1) ? gm_str(arg(c, 1)) : std::string();
        LOGW("mss32: AIL_set_named_sample_file(%s) failed", name.c_str());
    }
}

// AIL_WAV_info(void *file_image, AILSOUNDINFO *info). The structure is nine
// dwords: format, data_ptr, data_len, rate, bits, channels, samples,
// block_size and initial_ptr (Miles mss.h). RT3's loader copies it wholesale
// into its sound object and only the raw image reaches the sample-file call,
// so a wrong field is at worst a wrong reported length, never a misdecode.
void ail_wav_info(X86 *c) {
    uint32_t image = arg(c, 0), out = arg(c, 1);
    set_eax(c, 0);
    if (!image || !out || !gm_valid(out, 36))
        return;
    uint32_t bytes = sample_image_bytes(image, 0);
    gm_zero(out, 36);
    RiffWave w{};
    if (bytes && riff_parse_wave(image, bytes, &w)) {
        uint32_t block = (uint32_t)w.channels * (w.bits / 8u);
        wr32(out + 0, 1); // WAVE_FORMAT_PCM
        wr32(out + 4, w.pcm);
        wr32(out + 8, w.pcm_bytes);
        wr32(out + 12, w.rate);
        wr32(out + 16, w.bits);
        wr32(out + 20, w.channels);
        wr32(out + 24, block ? w.pcm_bytes / block : 0);
        wr32(out + 28, block);
        wr32(out + 32, w.pcm);
        set_eax(c, 1);
        return;
    }
    if (bytes) {
        Mp3Source source;
        if (source.open(std::vector<uint8_t>(g_mem + image, g_mem + image + bytes))) {
            wr32(out + 0, 0x55); // WAVE_FORMAT_MPEGLAYER3
            wr32(out + 4, image);
            wr32(out + 8, bytes);
            wr32(out + 12, source.rate());
            wr32(out + 16, 16);
            wr32(out + 20, source.channels());
            set_eax(c, 1);
        }
    }
}

void driver_release(Driver &d) {
    if (d.primary_buffer)
        com_release_internal(d.primary_buffer);
    if (d.ds_object)
        com_release_internal(d.ds_object);
    d.primary_buffer = nullptr;
    d.ds_object = nullptr;
}

// A successful waveOutOpen must populate HDIGDRIVER, not just return zero.
void ail_wave_open(X86 *c) {
    uint32_t out = arg(c, 0), fmt = arg(c, 3);
    set_eax(c, 1);
    if (!out || !gm_valid(out, 4))
        return;
    wr32(out, 0);
    if (!fmt || !gm_valid(fmt, 16) || rd16(fmt) != 1 || !rd32(fmt + 4) ||
        (rd16(fmt + 2) != 1 && rd16(fmt + 2) != 2) || (rd16(fmt + 14) != 8 && rd16(fmt + 14) != 16))
        return;
    uint32_t id = g_next_driver++;
    Driver &d = g_drivers[id];
    d.rate = rd32(fmt + 4);
    d.bits = rd16(fmt + 14);
    d.channels = rd16(fmt + 2);
    d.master = 127;
    g_last_digital_driver = id;
    wr32(out, id);
    if (arg(c, 1) && gm_valid(arg(c, 1), 4))
        wr32(arg(c, 1), id);
    LOGV("mss32: opened digital driver %08x (%u Hz)", id, rd32(fmt + 4));
    set_eax(c, 0);
}
// AIL_open_digital_driver(frequency, bits, channels, flags) returns the
// HDIGDRIVER directly rather than through an out parameter. RT3 opens the
// driver at the game's mixed rate (22050 Hz, 16-bit stereo). A zero rate
// selects Miles' default; the shim records the requested shape so
// digital_configuration and sample volume answer consistently.
void ail_open_digital_driver(X86 *c) {
    uint32_t rate = arg(c, 0), bits = arg(c, 1), channels = arg(c, 2), flags = arg(c, 3);
    if (channels > 2) {
        LOGW("mss32: AIL_open_digital_driver channels=%u needs the unsupported multi-channel path",
             channels);
        set_eax(c, 0);
        return;
    }
    if (flags) {
        LOGW("mss32: AIL_open_digital_driver flags=%08x are not supported", flags);
        set_eax(c, 0);
        return;
    }
    if (!rate)
        rate = 22050;
    if (bits != 8)
        bits = 16;
    if (channels != 1)
        channels = 2;
    ComObj *ds = dsound_new_device();
    if (!ds) {
        LOGW("mss32: AIL_open_digital_driver could not create a DirectSound device");
        set_eax(c, 0);
        return;
    }
    com_internalize(ds);
    ComObj *primary = dsound_new_primary_buffer(rate, bits, channels, DSBCAPS_PRIMARYBUFFER);
    if (!primary) {
        com_release_internal(ds);
        LOGW("mss32: AIL_open_digital_driver could not create a primary buffer");
        set_eax(c, 0);
        return;
    }
    com_internalize(primary);
    if (!com_view(primary, IF_DSBUFFER)) {
        LOGW("mss32: AIL_open_digital_driver could not create a primary buffer view");
        com_release_internal(primary);
        com_release_internal(ds);
        set_eax(c, 0);
        return;
    }
    uint32_t id = g_next_driver++;
    Driver &d = g_drivers[id];
    d.rate = rate;
    d.bits = bits;
    d.channels = channels;
    d.master = 127;
    d.ds_object = ds;
    d.primary_buffer = primary;
    g_last_digital_driver = id;
    LOGV("mss32: opened digital driver %08x (%u Hz, %u-bit, %u ch)", id, rate, bits, channels);
    set_eax(c, id);
}

void ail_set_digital_master_reverb(X86 *c) {
    uint32_t handle = arg(c, 0);
    if (handle) {
        auto it = g_drivers.find(handle);
        if (it == g_drivers.end()) {
            imports_unsupported(c);
            return;
        }
        float decay = bits_to_float(arg(c, 1)), delay = bits_to_float(arg(c, 2)),
              damping = bits_to_float(arg(c, 3));
        if (!std::isfinite(decay) || decay < 0 || decay > 100 || !std::isfinite(delay) ||
            delay < 0 || delay > 2 || !std::isfinite(damping) || damping < 0 || damping > 4) {
            LOGW("mss32: invalid master reverb decay=%g predelay=%g damping=%g", decay, delay,
                 damping);
            imports_unsupported(c);
            return;
        }
        for (int i = 0; i < 3; ++i)
            it->second.reverb[i] = arg(c, i + 1);
        driver_apply_mix(handle);
    }
    set_eax(c, 0);
}

void ail_digital_master_reverb(X86 *c) {
    uint32_t handle = arg(c, 0);
    if (handle) {
        auto it = g_drivers.find(handle);
        if (it == g_drivers.end()) {
            imports_unsupported(c);
            return;
        }
        for (int i = 0; i < 3; ++i) {
            uint32_t out = arg(c, i + 1);
            if (out)
                wr32(out, it->second.reverb[i]);
        }
    }
    set_eax(c, 0);
}

void ail_get_dsound_info(X86 *c) {
    uint32_t handle = arg(c, 0), out_ds = arg(c, 1), out_primary = arg(c, 2);
    Driver *default_driver = nullptr, *driver = nullptr;
    auto def = g_drivers.find(g_last_digital_driver);
    if (def != g_drivers.end())
        default_driver = &def->second;
    if (handle) {
        auto it = g_drivers.find(handle);
        if (it == g_drivers.end()) {
            LOGW("mss32: AIL_get_DirectSound_info asked for unknown driver %08x", handle);
            set_eax(c, 0);
            return;
        }
        driver = &it->second;
    } else {
        driver = default_driver;
    }
    if (!driver) {
        set_eax(c, 1);
        return;
    }
    Driver *primary_driver = handle ? default_driver : driver;
    if (out_primary && !primary_driver) {
        LOGW("mss32: AIL_get_DirectSound_info has no default driver for the primary buffer");
        set_eax(c, 0);
        return;
    }
    if (out_ds && !gm_valid(out_ds, 4)) {
        LOGW("mss32: AIL_get_DirectSound_info DirectSound output %08x is not guest memory", out_ds);
        set_eax(c, 0);
        return;
    }
    if (out_primary && !gm_valid(out_primary, 4)) {
        LOGW("mss32: AIL_get_DirectSound_info primary output %08x is not guest memory",
             out_primary);
        set_eax(c, 0);
        return;
    }
    if (out_ds)
        wr32(out_ds, driver->ds_object ? com_view(driver->ds_object, IF_DSOUND) : 0);
    if (out_primary)
        wr32(out_primary, com_view(primary_driver->primary_buffer, IF_DSBUFFER));
    set_eax(c, 1);
}

void ail_wave_close(X86 *c) {
    uint32_t id = arg(c, 0);
    stream_close_driver(id);
    for (auto &s : g_samples)
        if (s.alive && s.driver == id) {
            sample_reset(s);
            s.alive = false;
        }
    auto it = g_drivers.find(id);
    if (it != g_drivers.end()) {
        driver_release(it->second);
        host_audio_delete_reverb(id);
        g_drivers.erase(it);
    }
    if (g_last_digital_driver == id)
        g_last_digital_driver = 0;
    set_eax(c, 0);
}
void ail_configuration(X86 *c) {
    auto it = g_drivers.find(arg(c, 0));
    if (it != g_drivers.end()) {
        if (arg(c, 1) && gm_valid(arg(c, 1), 4))
            wr32(arg(c, 1), it->second.rate);
        if (arg(c, 2) && gm_valid(arg(c, 2), 4))
            wr32(arg(c, 2), (it->second.channels == 2 ? 2 : 0) | (it->second.bits == 16 ? 1 : 0));
        if (arg(c, 3) && gm_valid(arg(c, 3), 128))
            gm_put_str(arg(c, 3), "Native digital audio", 128);
    }
    set_eax(c, 0);
}
void ail_master_volume(X86 *c) {
    auto it = g_drivers.find(arg(c, 0));
    if (it != g_drivers.end())
        it->second.master = std::clamp((int32_t)arg(c, 1), 0, 127);
    driver_apply_mix(arg(c, 0));
    set_eax(c, 0);
}
void ail_preference(X86 *c) {
    uint32_t index = arg(c, 0);
    set_eax(c, index < 64 ? g_preferences[index] : 0);
    if (index < 64)
        g_preferences[index] = (int32_t)arg(c, 1);
}
void ail_get_preference(X86 *c) {
    set_eax(c, arg(c, 0) < 64 ? g_preferences[arg(c, 0)] : 0);
}

void ail_start_sample(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0)); s && s->wave.pcm_bytes) {
        if (s->channel < 0)
            s->channel = dx_alloc_audio_channel();
        if (s->channel >= 0) {
            s->remaining = s->loops;
            s->eos_fired = false;
            sample_play(*s);
        }
    }
    set_eax(c, 0);
}

void ail_end_sample(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0)))
        sample_stop(*s);
    set_eax(c, 0);
}

void ail_lock(X86 *c) {
    set_eax(c, 0);
}
void ail_unlock(X86 *c) {
    set_eax(c, 0);
}

void ail_stop_sample(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0)))
        sample_stop(*s);
    set_eax(c, 0);
}

// AIL_resume_sample continues a sample the engine stopped. The shim has no
// separate paused state, so a stopped sample with PCM restarts from its first
// channel and re-submits the image.
void ail_resume_sample(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0)); s && !s->playing && s->wave.pcm_bytes) {
        if (s->channel < 0)
            s->channel = dx_alloc_audio_channel();
        if (s->channel >= 0) {
            s->remaining = s->loops;
            s->eos_fired = false;
            sample_play(*s);
        }
    }
    set_eax(c, 0);
}

void ail_sample_status(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    Sample *s = sample_for(arg(c, 0));
    if (s && s->stream >= 0) {
        set_eax(c, stream_status((uint32_t)s->stream + 1));
        return;
    }
    if (s)
        sample_update(*s);
    set_eax(c, !s ? SMP_FREE : s->playing ? SMP_PLAYING : SMP_DONE);
}

void ail_set_sample_volume(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0))) {
        s->volume01 = std::clamp((int32_t)arg(c, 1), 0, 127) / 127.0f;
        sample_apply_mix(*s);
    }
    set_eax(c, 0);
}

void ail_set_sample_pan(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0))) {
        int32_t p = std::clamp((int32_t)arg(c, 1), 0, 127);
        s->pan01 = p <= 64 ? p / 128.0f : 0.5f + (p - 64) / 126.0f;
        sample_apply_mix(*s);
    }
    set_eax(c, 0);
}
void ail_sample_volume(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    set_eax(c, s ? (uint32_t)std::lround(s->volume01 * 127.0f) : 0);
}
void ail_sample_pan(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    float p = s ? s->pan01 : 0.5f;
    set_eax(c, (uint32_t)std::lround(p <= 0.5f ? p * 128.0f : 64.0f + (p - 0.5f) * 126.0f));
}
void ail_digital_master_volume(X86 *c) {
    auto it = g_drivers.find(arg(c, 0));
    set_eax(c, it != g_drivers.end() ? (uint32_t)it->second.master : 0);
}

// AIL_set_sample_type(HSAMPLE, format, flags) describes raw PCM that
// AIL_set_sample_address then points at: DIG_F_MONO_8, MONO_16, STEREO_8 or
// STEREO_16, unsigned when 8-bit. DIG_PCM_SIGN (1) is what 16-bit PCM already
// is; signed 8-bit, DIG_PCM_ORDER and ADPCM are not modelled.
void ail_set_sample_type(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    uint32_t format = arg(c, 1), flags = arg(c, 2);
    if (s && (format > 3 || (flags & ~(format & 1 ? 1u : 0u)))) {
        LOGW("mss32: AIL_set_sample_type format %u flags %u is unsupported", format, flags);
        imports_unsupported(c);
        return;
    }
    if (s) {
        sample_stop(*s);
        s->decoded.clear();
        s->wave.channels = format & 2 ? 2 : 1;
        s->wave.bits = format & 1 ? 16 : 8;
        if (!s->wave.rate)
            s->wave.rate = 11025;
    }
    set_eax(c, 0);
}
void ail_set_sample_address(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    uint32_t start = arg(c, 1), bytes = arg(c, 2);
    if (s) {
        sample_stop(*s);
        s->decoded.clear();
        bool ok = start && bytes && gm_valid(start, bytes);
        s->wave.pcm = ok ? start : 0;
        s->wave.pcm_bytes = ok ? bytes : 0;
        if (!s->wave.bits) {
            s->wave.bits = 8;
            s->wave.channels = 1;
            s->wave.rate = 11025;
        }
    }
    set_eax(c, 0);
}

void ail_set_sample_loop_count(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0)))
        s->loops = arg(c, 1);
    set_eax(c, 0);
}

void ail_sample_loop_count(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    set_eax(c, s ? s->loops : 0);
}

uint32_t float_to_bits(float v) {
    uint32_t bits;
    memcpy(&bits, &v, 4);
    return bits;
}

// AIL_set_sample_volume_levels(HSAMPLE, F32 left, F32 right) and
// AIL_set_sample_volume_pan(HSAMPLE, F32 volume, F32 pan) are the two forms
// the engine's 2D path uses. The shim's channel has one volume and one pan,
// so the two levels collapse to their loudest and their balance, and the
// combined form retains its normalized volume and pan values.
void ail_set_sample_volume_levels(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    set_eax(c, 0);
    if (!s)
        return;
    float l = std::clamp(bits_to_float(arg(c, 1)), 0.0f, 1.0f);
    float r = std::clamp(bits_to_float(arg(c, 2)), 0.0f, 1.0f);
    float loud = l > r ? l : r;
    float sum = l + r;
    s->volume01 = loud;
    s->pan01 = sum > 0.0f ? r / sum : 0.5f;
    sample_apply_mix(*s);
}

void ail_set_sample_volume_pan(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    set_eax(c, 0);
    if (!s)
        return;
    float v = std::clamp(bits_to_float(arg(c, 1)), 0.0f, 1.0f);
    float p = std::clamp(bits_to_float(arg(c, 2)), 0.0f, 1.0f);
    s->volume01 = v;
    s->pan01 = p;
    sample_apply_mix(*s);
}

void ail_set_sample_reverb_levels(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    if (!s)
        return;
    float dry = bits_to_float(arg(c, 1)), wet = bits_to_float(arg(c, 2));
    if (!std::isfinite(dry) || dry < 0.0f || !std::isfinite(wet) || wet < 0.0f) {
        LOGW("mss32: AIL_set_sample_reverb_levels unsupported sample=%08x dry=%g wet=%g", arg(c, 0),
             dry, wet);
        imports_unsupported(c);
        return;
    }
    s->reverb_dry = arg(c, 1);
    s->reverb_wet = arg(c, 2);
    sample_apply_mix(*s);
}

void ail_sample_reverb_levels(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    if (!s)
        return;
    if (arg(c, 1))
        wr32(arg(c, 1), s->reverb_dry);
    if (arg(c, 2))
        wr32(arg(c, 2), s->reverb_wet);
}

void ail_set_sample_playback_rate(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    if (Sample *s = sample_for(arg(c, 0))) {
        s->rate = arg(c, 1);
        if (s->channel >= 0)
            host_audio_set_frequency(s->channel, s->rate);
    }
    set_eax(c, 0);
}

void ail_register_eos_callback(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0))) {
        s->eos_cb = arg(c, 1);
        s->eos_fired = false;
    }
    set_eax(c, 0);
}

struct StreamSource {
    Mp3Source mp3;
    WavSource wav;
    bool wave = false;

    bool open(const std::vector<uint8_t> &bytes) {
        wave = bytes.size() >= 4 && bytes[0] == 'R' && bytes[1] == 'I' && bytes[2] == 'F' &&
               bytes[3] == 'F';
        return wave ? wav.open(bytes) : mp3.open(bytes);
    }
    bool decode_next(std::vector<int16_t> &out) {
        return wave ? wav.decode_next(out) : mp3.decode_next(out);
    }
    uint32_t rate() const {
        return wave ? wav.rate() : mp3.rate();
    }
    uint32_t channels() const {
        return wave ? wav.channels() : mp3.channels();
    }
    void seek_frames(uint64_t target) {
        if (wave)
            wav.seek_frames(target);
        else
            mp3.seek_frames(target);
    }
    bool drained() const {
        return wave ? wav.drained() : mp3.drained();
    }
    bool failed() const {
        return wave && wav.failed();
    }
    uint64_t total_frames() {
        int64_t mp3_frames = wave ? 0 : mp3.duration_frames();
        return wave ? wav.total_frames() : (uint64_t)std::max<int64_t>(mp3_frames, 0);
    }
};

// Stream handles own the encoded image, decoder and shared host channel. A
// host worker refills them; the guest frame seam retains timers and callbacks.
struct Stream {
    bool alive = false, playing = false;
    uint64_t generation = 0, last_refill_ns = 0;
    uint64_t refill_underruns = 0, max_refill_interval_ns = 0;
    uint32_t driver = 0;
    std::string name;
    // A paused stream is one the host voice has been stopped for. The host has
    // no pause of its own, so resuming restarts the stream; a title that pauses
    // to close does not care, and one that pauses for focus will hear the track
    // restart.
    bool paused = false;
    uint32_t sample = 0;
    int32_t channel = -1;
    uint32_t loops = 1, remaining = 1;
    uint64_t submitted = 0;
    // Where the current play started in the track, and where the next start
    // begins (AIL_set_stream_ms_position), in frames.
    uint64_t base_frames = 0, start_frames = 0;
    StreamSource source;
    std::vector<int16_t> pending;
    size_t pending_pos = 0;
};
std::vector<Stream> g_streams(64);
uint64_t g_stream_generation = 0;

void stream_report_refills(Stream &s) {
    if (!s.last_refill_ns)
        return;
    log_msg(s.refill_underruns ? 1 : 2,
            "mss32: stream refills name=%s generation=%llu underruns=%llu "
            "max_refill_interval_ms=%llu",
            s.name.c_str(), (unsigned long long)s.generation,
            (unsigned long long)s.refill_underruns,
            (unsigned long long)(s.max_refill_interval_ns / 1000000ull));
    s.last_refill_ns = 0;
    s.refill_underruns = 0;
    s.max_refill_interval_ns = 0;
}

void stream_stop_alias(int32_t index) {
    if (index >= 0 && (size_t)index < g_streams.size()) {
        stream_report_refills(g_streams[index]);
        g_streams[index].playing = false;
    }
}

void driver_apply_mix(uint32_t driver) {
    auto it = g_drivers.find(driver);
    if (it != g_drivers.end()) {
        Driver &d = it->second;
        host_audio_set_reverb(driver, bits_to_float(d.reverb[0]), bits_to_float(d.reverb[1]),
                              bits_to_float(d.reverb[2]));
    }
    for (auto &s : g_samples)
        if (s.alive && s.driver == driver)
            sample_apply_mix(s);
}

void ail_set_digital_master_reverb_levels(X86 *c) {
    uint32_t handle = arg(c, 0);
    if (handle) {
        auto it = g_drivers.find(handle);
        float dry = bits_to_float(arg(c, 1)), wet = bits_to_float(arg(c, 2));
        if (it == g_drivers.end() || !(dry >= 0.0f && dry <= 1.0f) ||
            !(wet >= 0.0f && wet <= 1.0f)) {
            LOGW(
                "mss32: AIL_set_digital_master_reverb_levels unsupported driver=%08x dry=%g wet=%g",
                handle, dry, wet);
            imports_unsupported(c);
            return;
        }
        it->second.reverb_dry = arg(c, 1);
        it->second.reverb_wet = arg(c, 2);
        driver_apply_mix(handle);
    }
    set_eax(c, 1);
}

void ail_digital_master_reverb_levels(X86 *c) {
    uint32_t handle = arg(c, 0);
    if (handle) {
        auto it = g_drivers.find(handle);
        if (it == g_drivers.end()) {
            imports_unsupported(c);
            return;
        }
        if (arg(c, 1))
            wr32(arg(c, 1), it->second.reverb_dry);
        if (arg(c, 2))
            wr32(arg(c, 2), it->second.reverb_wet);
    }
    set_eax(c, 1);
}

void ail_set_room_type(X86 *c) {
    static const uint32_t presets[][4] = {
        {0x00000000, 0x3fbf1aa0, 0x3f000000, 0x3c343958},
        {0x3e800000, 0x3dcccccd, 0x3dcccccd, 0x3b03126f},
        {0x3ed58106, 0x3ecccccd, 0x3f2a7efa, 0x3b449ba6},
        {0x3f400000, 0x3fbfdf3b, 0x3e29fbe7, 0x3c343958},
        {0x3e54fdf4, 0x3ef4bc6a, 0x3dcccccd, 0x3b83126f},
        {0x3e99999a, 0x4013c6a8, 0x3f2e147b, 0x3c8b4396},
        {0x3ece5604, 0x4088ed91, 0x3f000000, 0x3cf5c28f},
        {0x3f000000, 0x407d8106, 0x3f000000, 0x3ced9168},
        {0x3f000000, 0x4038b439, 0x3fa6e979, 0x3cb43958},
        {0x3eb8d4fe, 0x40e91687, 0x3ea9fbe7, 0x3cf5c28f},
        {0x3f000000, 0x41200000, 0x3e99999a, 0x3cf5c28f},
        {0x3e1cac08, 0x3e849ba6, 0x3f0ccccd, 0x3cf5c28f},
        {0x3eb8d4fe, 0x3fbf1aa0, 0x3f000000, 0x3c343958},
        {0x3ee353f8, 0x402c9ba6, 0x3f2353f8, 0x3ca3d70a},
        {0x3e800000, 0x3fe04189, 0x3f46a7f0, 0x3c343958},
        {0x3de353f8, 0x404947ae, 0x3ef1a9fc, 0x3db43958},
        {0x3de353f8, 0x40311687, 0x3e656042, 0x3c343958},
        {0x3e4ccccd, 0x3f4ccccd, 0x3e4ccccd, 0x3e99999a},
        {0x3f0ccccd, 0x40400000, 0x3f0ccccd, 0x3d3020c5},
        {0x3df5c28f, 0x3f4ccccd, 0x3e3851ec, 0x3dcccccd},
        {0x3e54fdf4, 0x3fd374bc, 0x3fc00000, 0x3c449ba6},
        {0x3f26e979, 0x4038b439, 0x3e800000, 0x3cac0831},
        {0x3f800000, 0x3fbfdf3b, 0x3dcccccd, 0x3c343958},
        {0x3f600000, 0x410645a2, 0x3fb1a9fc, 0x3c343958},
        {0x3e0e5604, 0x41200000, 0x3fc00000, 0x3c343958},
        {0x3ef8d4fe, 0x40f20419, 0x40200000, 0x3c343958},
    };
    auto it = g_drivers.find(arg(c, 0));
    uint32_t room = arg(c, 1);
    if (it == g_drivers.end() || room >= std::size(presets)) {
        imports_unsupported(c);
        return;
    }
    Driver &d = it->second;
    d.room_type = room;
    d.reverb_dry = 0x3f800000;
    d.reverb_wet = presets[room][0];
    d.reverb[0] = presets[room][1];
    d.reverb[1] = presets[room][3];
    d.reverb[2] = presets[room][2];
    driver_apply_mix(arg(c, 0));
    set_eax(c, 1);
}

void ail_room_type(X86 *c) {
    auto it = g_drivers.find(arg(c, 0));
    if (it == g_drivers.end()) {
        imports_unsupported(c);
        return;
    }
    set_eax(c, it->second.room_type);
}

Stream *stream_for(uint32_t handle) {
    if (!handle || handle > g_streams.size())
        return nullptr;
    Stream &s = g_streams[handle - 1];
    return s.alive ? &s : nullptr;
}

void stream_release(Stream &s) {
    stream_report_refills(s);
    if (Sample *h = sample_for(s.sample)) {
        *h = Sample{};
    }
    if (s.channel >= 0) {
        host_audio_stop(s.channel);
        dx_free_audio_channel(s.channel);
    }
    s = Stream{};
}

void stream_close_driver(uint32_t driver) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    for (auto &s : g_streams)
        if (s.alive && s.driver == driver)
            stream_release(s);
}

// Read through the runtime resolver, sharing CreateFileA's overlays, drive
// mapping and case folding. Open does not play until AIL_start_stream.
void ail_open_stream(X86 *c) {
    set_eax(c, 0);
    uint32_t name = arg(c, 1);
    if (!name || !gm_valid(name, 1)) {
        LOGW("mss32: open_stream: guest filename %08x is not readable", name);
        return;
    }
    std::string guest = gm_str(name);
    std::string path = win32_host_path_op(guest, WIN32_FILE_READ);
    int fd = path.empty() ? -1 : os_fd_open(path.c_str(), OS_O_RDONLY);
    if (fd < 0) {
        LOGW("mss32: open_stream(%s): cannot open %s", guest.c_str(), path.c_str());
        return;
    }
    OsStat st{};
    if (os_fd_stat(fd, &st) != 0 || !st.is_regular || !st.size || st.size > UINT32_MAX) {
        LOGW("mss32: open_stream(%s): cannot stat %s", guest.c_str(), path.c_str());
        os_fd_close(fd);
        return;
    }
    std::vector<uint8_t> bytes((size_t)st.size);
    size_t got = 0;
    while (got < bytes.size()) {
        int64_t n = os_fd_read(fd, bytes.data() + got, bytes.size() - got);
        if (n <= 0)
            break;
        got += (size_t)n;
    }
    os_fd_close(fd);
    if (got != bytes.size()) {
        LOGW("mss32: open_stream(%s): short read of %s", guest.c_str(), path.c_str());
        return;
    }
    StreamSource source;
    if (!source.open(bytes)) {
        WavImage wav{};
        if (riff_parse_wave_host(bytes.data(), bytes.size(), &wav))
            LOGW("mss32: open_stream(%s): WAVE format %04x bits=%u could not be decoded: %s",
                 guest.c_str(), wav.format, wav.bits, path.c_str());
        else
            LOGW("mss32: open_stream(%s): not a decodable WAVE or MPEG image: %s", guest.c_str(),
                 path.c_str());
        return;
    }
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    if (g_stream_host_shutdown) {
        LOGW("mss32: open_stream(%s) ignored after audio shutdown", guest.c_str());
        return;
    }
    if (!stream_worker_start()) {
        LOGW("mss32: open_stream(%s): cannot create stream worker", guest.c_str());
        return;
    }
    for (uint32_t i = 0; i < g_streams.size(); ++i) {
        Stream &s = g_streams[i];
        if (s.alive)
            continue;
        s = Stream{};
        s.source = std::move(source);
        s.channel = dx_alloc_audio_channel();
        if (s.channel < 0) {
            LOGW("mss32: open_stream(%s): no free audio channel", guest.c_str());
            s = Stream{};
            return;
        }
        s.alive = true;
        s.driver = arg(c, 0);
        s.name = guest;
        host_audio_set_frequency(s.channel, 0);
        for (uint32_t j = 0; j < g_samples.size() && !s.sample; ++j) {
            Sample &h = g_samples[j];
            if (h.alive)
                continue;
            h = Sample{};
            h.handle = j + 1;
            h.driver = s.driver;
            h.channel = s.channel;
            h.wave.rate = s.source.rate();
            h.wave.channels = (uint16_t)s.source.channels();
            h.wave.bits = 16;
            h.stream = (int32_t)i;
            h.alive = true;
            s.sample = h.handle;
        }
        if (!s.sample) {
            LOGW("mss32: open_stream(%s): no free sample handle", guest.c_str());
            host_audio_stop(s.channel);
            dx_free_audio_channel(s.channel);
            s = Stream{};
            return;
        }
        LOGV("mss32: open_stream(%s): %s, %u Hz, %u channel(s)", guest.c_str(), path.c_str(),
             s.source.rate(), s.source.channels());
        set_eax(c, i + 1);
        return;
    }
    LOGW("mss32: open_stream(%s): no free stream handle", guest.c_str());
}

// Decoder EOF is not playback EOF:
// status stays playing until the host consumes the first play and all queues.
void stream_update(Stream &s) {
    if (g_stream_host_shutdown || !s.playing)
        return;
    uint32_t ahead =
        std::max(1u, s.source.rate() * STREAM_BUFFER_MS / 1000) * s.source.channels() * 2;
    uint32_t queued = 0;
    while (queued < ahead && host_audio_queued_bytes(s.channel) < ahead) {
        if (s.pending.empty()) {
            if (!s.source.decode_next(s.pending)) {
                if (s.source.failed())
                    break;
                if (s.loops != 0 && s.remaining <= 1)
                    break;
                if (s.remaining > 1)
                    --s.remaining;
                s.source.seek_frames(0);
                if (!s.source.decode_next(s.pending))
                    break;
            }
            s.pending_pos = 0;
        }
        uint32_t bytes = (uint32_t)(s.pending.size() - s.pending_pos) * 2;
        bool dry = s.submitted && host_audio_voice_remaining_bytes(s.channel) == 0;
        int32_t taken = host_audio_queue(s.channel, s.pending.data() + s.pending_pos, bytes);
        if (taken <= 0)
            return;
        uint64_t now = os_monotonic_ns();
        uint64_t refill_interval = s.last_refill_ns ? now - s.last_refill_ns : 0;
        s.max_refill_interval_ns = std::max(s.max_refill_interval_ns, refill_interval);
        if (dry && refill_interval) {
            ++s.refill_underruns;
            LOGW("mss32: stream refill after dry gap name=%s generation=%llu "
                 "refill_interval_ms=%llu",
                 s.name.c_str(), (unsigned long long)s.generation,
                 (unsigned long long)(refill_interval / 1000000ull));
        }
        s.last_refill_ns = now;
        s.submitted += (uint32_t)taken;
        queued += (uint32_t)taken;
        s.pending_pos += (uint32_t)taken / 2;
        if (s.pending_pos >= s.pending.size())
            s.pending.clear();
    }
    if (s.source.drained() && s.pending.empty() &&
        host_audio_played_bytes(s.channel) >= s.submitted)
        s.playing = false;
}

void *stream_worker(void *) {
    std::unique_lock<std::mutex> lock(g_stream_mutex);
    while (!g_stream_worker_stop) {
        g_stream_condition.wait_for(lock, std::chrono::milliseconds(10),
                                    [] { return g_stream_worker_stop || g_stream_command; });
        if (g_stream_worker_stop)
            break;
        g_stream_command = false;
        for (auto &s : g_streams)
            if (s.alive)
                stream_update(s);
    }
    return nullptr;
}

bool stream_worker_start() {
    if (g_stream_host_shutdown)
        return false;
    if (g_stream_thread)
        return true;
    g_stream_worker_stop = false;
    g_stream_command = true;
    g_stream_thread = os_thread_create(stream_worker, nullptr, 0);
    if (!g_stream_thread) {
        g_stream_worker_stop = true;
        return false;
    }
    return true;
}

// Prime before starting the host voice. Starting again rewinds.
void stream_start(Stream &s) {
    if (g_stream_host_shutdown) {
        LOGW("mss32: stream start(%s) ignored after audio shutdown", s.name.c_str());
        return;
    }
    host_audio_stop(s.channel);
    stream_report_refills(s);
    s.playing = false;
    s.paused = false;
    s.pending.clear();
    s.source.seek_frames(s.start_frames);
    s.base_frames = s.start_frames;
    s.start_frames = 0;
    s.remaining = s.loops;
    s.generation = ++g_stream_generation;
    std::vector<int16_t> decoded;
    const size_t prime_samples =
        (size_t)std::max(1u, s.source.rate() * STREAM_BUFFER_MS / 1000) * s.source.channels();
    while (s.pending.size() < prime_samples) {
        decoded.clear();
        if (s.source.decode_next(decoded)) {
            s.pending.insert(s.pending.end(), decoded.begin(), decoded.end());
            continue;
        }
        if (s.source.failed() || (s.loops != 0 && s.remaining <= 1))
            break;
        if (s.remaining > 1)
            --s.remaining;
        s.source.seek_frames(0);
        if (!s.source.decode_next(decoded))
            break;
        s.pending.insert(s.pending.end(), decoded.begin(), decoded.end());
    }
    if (!s.pending.empty()) {
        HostAudioPlay p{};
        p.channel = s.channel;
        p.pcm = s.pending.data();
        p.bytes = (uint32_t)s.pending.size() * 2;
        p.sample_rate = (int32_t)s.source.rate();
        p.channels = (int32_t)s.source.channels();
        p.bits = 16;
        Sample *h = sample_for(s.sample);
        if (!h) {
            LOGW("mss32: stream start(%s): stream has no sample handle", s.name.c_str());
            return;
        }
        host_audio_set_frequency(s.channel, h->rate);
        p.volume = sample_volume(*h);
        p.pan = h->is_3d ? sample3d_pan_mb(*h) : miles_pan_mb(h->pan01);
        host_audio_play(&p);
        s.submitted = p.bytes;
        s.last_refill_ns = os_monotonic_ns();
        s.pending.clear();
        s.pending_pos = 0;
        s.playing = host_audio_stream(s.channel) >= 0;
        sample_apply_mix(*h);
        if (!s.playing)
            host_audio_stop(s.channel);
        else {
            g_stream_command = true;
            g_stream_condition.notify_one();
        }
    } else {
        LOGW("mss32: stream start(%s): produced no PCM", s.name.c_str());
    }
}

void ail_start_stream(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    if (Stream *s = stream_for(arg(c, 0)))
        stream_start(*s);
    else
        LOGW("mss32: AIL_start_stream(%08x) has no stream", arg(c, 0));
    set_eax(c, 0);
}

void ail_close_stream(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    if (Stream *s = stream_for(arg(c, 0)))
        stream_release(*s);
    set_eax(c, 0);
}

uint32_t stream_status(uint32_t handle) {
    Stream *s = stream_for(handle);
    if (s && s->paused)
        return 0x8u;
    if (s)
        stream_update(*s);
    return s && s->playing ? SMP_PLAYING : SMP_DONE;
}

void ail_stream_status(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    set_eax(c, stream_status(arg(c, 0)));
}

void ail_set_stream_volume(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    if (Stream *s = stream_for(arg(c, 0)))
        if (Sample *h = sample_for(s->sample)) {
            h->volume01 = std::clamp((int32_t)arg(c, 1), 0, 127) / 127.0f;
            sample_apply_mix(*h);
        }
    set_eax(c, 0);
}

void ail_stream_volume(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    Stream *s = stream_for(arg(c, 0));
    Sample *h = s ? sample_for(s->sample) : nullptr;
    set_eax(c, h ? (uint32_t)std::lround(h->volume01 * 127.0f) : 0);
}

void ail_set_stream_loop_count(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    if (Stream *s = stream_for(arg(c, 0)))
        s->remaining = s->loops = arg(c, 1);
    g_stream_command = true;
    g_stream_condition.notify_one();
    set_eax(c, 0);
}

// AIL_pause_stream(HSTREAM, S32 pause). The host voice is stopped on pause and
// the stream restarted on resume; SMP_STOPPED from the status call is what
// tells the engine it may resume.
void ail_pause_stream(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    Stream *s = stream_for(arg(c, 0));
    if (s && arg(c, 1)) {
        stream_report_refills(*s);
        if (s->playing || s->channel >= 0)
            host_audio_stop(s->channel);
        s->playing = false;
        s->paused = true;
    } else if (s) {
        // Resume by restarting: the host has no pause to undo.
        s->paused = false;
        if (!s->playing)
            stream_start(*s);
    }
    set_eax(c, 0);
}

// AIL_set_stream_volume_levels(HSTREAM, F32 left, F32 right): the two levels
// collapse to the host channel's single volume, as the sample form does.
void ail_set_stream_volume_levels(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    if (Stream *s = stream_for(arg(c, 0)))
        if (Sample *h = sample_for(s->sample)) {
            float l = std::clamp(bits_to_float(arg(c, 1)), 0.0f, 1.0f);
            float r = std::clamp(bits_to_float(arg(c, 2)), 0.0f, 1.0f);
            float sum = l + r;
            h->volume01 = std::max(l, r);
            h->pan01 = sum > 0.0f ? r / sum : 0.5f;
            sample_apply_mix(*h);
        }
    set_eax(c, 0);
}

// ---------------------------------------------------------------------------
// Miles 3D providers, listener and positional samples.
//
// RT3 will not initialise audio at all without a 3D provider: its
// MusicSystem::initializeMilesAudio opens one by name and allocates 32 3D
// sample handles, and a zero provider count makes that path return failure and
// the settings UI report "This computer has no sound capabilities." (language
// string 3130). Miles always enumerates its software positional providers on a
// machine without 3D hardware, so the shim does too. The host mixer is 2D; a
// 3D sample's listener-relative position drives pan and distance gain through
// the model shared with DirectSound3D in audio3d.h.
// ---------------------------------------------------------------------------
struct Provider3D {
    const char *name;
    uint32_t handle;
};
const Provider3D g_providers3d[] = {
    {"DirectSound3D 7+ Software - Pan and Volume", 0x00020001u},
};
uint32_t g_provider_names3d = 0;

void ail_enumerate_3D_providers(X86 *c) {
    uint32_t cursor = arg(c, 0), next = arg(c, 1), name = arg(c, 2);
    set_eax(c, 0);
    if (!cursor || !next || !name || !gm_valid(cursor, 4))
        return;
    // The title keeps the char* Miles hands it and compares it against later
    // enumerations, so the names live in guest memory, not host string
    // literals, and stay put for the run.
    if (!g_provider_names3d) {
        uint32_t block = ail_alloc(c, 128);
        set_eax(c, 0);
        if (!block)
            return;
        g_provider_names3d = block;
        for (size_t i = 0; i < std::size(g_providers3d); ++i)
            gm_put_str(block + (uint32_t)i * 64, g_providers3d[i].name, 64);
    }
    uint32_t idx = rd32(cursor);
    if (idx >= std::size(g_providers3d))
        return;
    wr32(next, g_providers3d[idx].handle);
    wr32(name, g_provider_names3d + idx * 64);
    wr32(cursor, idx + 1);
    set_eax(c, 1);
}

// AIL_3D_provider_attribute(HPROVIDER, name, void *value). The software provider
// voices 3D samples from the shared pool, leaving half of it for 2D sounds; it
// has no EAX attributes, so those leave the value untouched as Miles does.
void ail_3D_provider_attribute(X86 *c) {
    uint32_t out = arg(c, 2);
    std::string name = arg(c, 1) ? gm_str(arg(c, 1)) : std::string();
    if (name == "Maximum supported samples" && out && gm_valid(out, 4))
        wr32(out, (uint32_t)g_samples.size() / 2);
    else
        log_once("mss32.3dattr", "mss32: AIL_3D_provider_attribute(%s) is not a provider attribute",
                 name.c_str());
    set_eax(c, 0);
}

// AIL_open_3D_provider returns zero on success; anything else is a Miles error
// code the engine reads as "provider unavailable". The one software provider
// always opens.
void ail_open_3D_provider(X86 *c) {
    set_eax(c, 0);
}

void ail_3D_speaker_type(X86 *c) {
    set_eax(c, 0); // AIL_3D_HEADPHONE; the host mix is stereo
}
void ail_set_3D_speaker_type(X86 *c) {
    set_eax(c, 0);
}
// A room type of -1 means "no environmental reverb", which is what a host
// mixer with no effect chain honestly reports.
void ail_3D_room_type(X86 *c) {
    set_eax(c, 0xffffffffu);
}
void ail_set_3D_distance_factor(X86 *c) {
    g_rolloff3d = bits_to_float(arg(c, 1));
    set_eax(c, 0);
}

void ail_open_3D_listener(X86 *c) {
    set_eax(c, kListener3DHandle);
}
void ail_close_3D_listener(X86 *c) {
    set_eax(c, 0);
}

void ail_allocate_3D_sample_handle(X86 *c) {
    set_eax(c, 0);
    for (uint32_t i = 0; i < g_samples.size(); ++i) {
        Sample &s = g_samples[i];
        if (s.alive)
            continue;
        s = Sample{};
        s.handle = i + 1;
        s.driver = g_last_digital_driver;
        s.alive = true;
        s.is_3d = true;
        set_eax(c, s.handle);
        return;
    }
}

void ail_release_3D_sample_handle(X86 *c) {
    ail_release_sample_handle(c);
}

// AIL_set_3D_sample_file(H3DSAMPLE, void *file_image). The engine hands it the
// same in-memory image its loader already read, so the shared decoder is used.
void ail_set_3D_sample_file(X86 *c) {
    set_eax(c, 0);
    Sample *s = sample_for(arg(c, 0));
    if (!s)
        return;
    sample_stop(*s);
    uint32_t bytes = sample_image_bytes(arg(c, 1), 0);
    if (sample_load_image(*s, arg(c, 1), bytes))
        set_eax(c, 1);
}

void ail_set_3D_sample_loop_count(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0)))
        s->loops = arg(c, 1);
    set_eax(c, 0);
}

// Miles 6 takes an S32 volume 0..127, Miles 7 an F32 0..1. As float bits, every
// value up to 127 is a denormal no Miles 7 caller passes, so the two agree.
void ail_set_3D_sample_volume(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0))) {
        uint32_t v = arg(c, 1);
        s->volume01 = v <= 127 ? v / 127.0f : std::clamp(bits_to_float(v), 0.0f, 1.0f);
        sample_apply_mix(*s);
    }
    set_eax(c, 0);
}

// AIL_set_3D_sample_distances(H3DSAMPLE, F32 max, F32 min).
void ail_set_3D_sample_distances(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0))) {
        s->max_d = bits_to_float(arg(c, 1));
        s->min_d = bits_to_float(arg(c, 2));
        sample_apply_mix(*s);
    }
    set_eax(c, 0);
}

void ail_set_3D_sample_playback_rate(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    if (Sample *s = sample_for(arg(c, 0))) {
        s->rate = arg(c, 1);
        if (s->channel >= 0 && s->rate)
            host_audio_set_frequency(s->channel, s->rate);
    }
    set_eax(c, 0);
}

// Effects level and occlusion are accepted and not applied: the host mix has
// no effect chain, and RT3 does not drive a source orientation that would
// change the pan. Each is reported once so a future title that depends on one
// is not silent about it.
void ail_set_3D_sample_effects_level(X86 *c) {
    log_once("mss32.3dfx",
             "mss32: a 3D sample effects level is set but the host mix applies no effects");
    set_eax(c, 0);
}
void ail_set_3D_sample_occlusion(X86 *c) {
    log_once("mss32.3doccl", "mss32: 3D sample occlusion is set but is not applied to the mix");
    if (Sample *s = sample_for(arg(c, 0)))
        s->occlusion = bits_to_float(arg(c, 1));
    set_eax(c, 0);
}
void ail_set_3D_sample_obstruction(X86 *c) {
    log_once("mss32.3dobst", "mss32: 3D sample obstruction is set but is not applied to the mix");
    if (Sample *s = sample_for(arg(c, 0)))
        s->obstruction = bits_to_float(arg(c, 1));
    set_eax(c, 0);
}
void ail_3D_sample_occlusion(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    fpush(c, s ? s->occlusion : 0.0);
}
void ail_3D_sample_obstruction(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    fpush(c, s ? s->obstruction : 0.0);
}
void ail_3D_sample_playback_rate(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    set_eax(c, !s ? 0 : s->rate ? s->rate : s->wave.rate);
}

void put_floats(X86 *c, int first, const float *values, int count) {
    for (int i = 0; i < count; ++i)
        if (uint32_t p = arg(c, first + i); p && gm_valid(p, 4))
            wr32(p, float_to_bits(values[i]));
}

// AIL_3D_position/orientation/velocity(H3DPOBJECT, F32 *...) read back what
// the setters stored, for the listener or a 3D sample.
void ail_3D_position(X86 *c) {
    if (arg(c, 0) == kListener3DHandle)
        put_floats(c, 1, g_listener_pos3d, 3);
    else if (Sample *s = sample_for(arg(c, 0)))
        put_floats(c, 1, s->pos3d, 3);
    set_eax(c, 0);
}
void ail_3D_orientation(X86 *c) {
    if (arg(c, 0) == kListener3DHandle) {
        put_floats(c, 1, g_listener_front3d, 3);
        put_floats(c, 4, g_listener_top3d, 3);
    } else if (Sample *s = sample_for(arg(c, 0))) {
        put_floats(c, 1, s->front3d, 3);
        put_floats(c, 4, s->up3d, 3);
    }
    set_eax(c, 0);
}
void ail_3D_velocity(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0)))
        put_floats(c, 1, s->vel3d, 3);
    set_eax(c, 0);
}
void ail_set_3D_velocity_vector(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0)))
        for (int i = 0; i < 3; ++i)
            s->vel3d[i] = bits_to_float(arg(c, 1 + i));
    set_eax(c, 0);
}

// Miles keeps eight user words on every sample handle, 2D or 3D.
void ail_set_sample_user_data(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0)); s && arg(c, 1) < 8)
        s->user[arg(c, 1)] = arg(c, 2);
    set_eax(c, 0);
}
void ail_sample_user_data(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    set_eax(c, s && arg(c, 1) < 8 ? s->user[arg(c, 1)] : 0);
}

void ail_set_3D_position(X86 *c) {
    uint32_t handle = arg(c, 0);
    float x = bits_to_float(arg(c, 1)), y = bits_to_float(arg(c, 2)), z = bits_to_float(arg(c, 3));
    if (handle == kListener3DHandle) {
        g_listener_pos3d[0] = x;
        g_listener_pos3d[1] = y;
        g_listener_pos3d[2] = z;
        sample_update_3d_mix();
    } else if (Sample *s = sample_for(handle)) {
        s->pos3d[0] = x;
        s->pos3d[1] = y;
        s->pos3d[2] = z;
        sample_apply_mix(*s);
    }
    set_eax(c, 0);
}

void ail_set_3D_orientation(X86 *c) {
    uint32_t handle = arg(c, 0);
    if (handle == kListener3DHandle) {
        for (int i = 0; i < 3; ++i) {
            g_listener_front3d[i] = bits_to_float(arg(c, 1 + i));
            g_listener_top3d[i] = bits_to_float(arg(c, 4 + i));
        }
        // Every source's pan is measured against the listener, so a listener
        // move re-places anything already playing.
        sample_update_3d_mix();
    }
    set_eax(c, 0);
}

void ail_start_3D_sample(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0)); s && s->wave.pcm_bytes) {
        if (s->channel < 0)
            s->channel = dx_alloc_audio_channel();
        if (s->channel >= 0) {
            s->remaining = s->loops;
            s->eos_fired = false;
            sample_play(*s);
        }
    }
    set_eax(c, 0);
}
void ail_stop_3D_sample(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0)))
        sample_stop(*s);
    set_eax(c, 0);
}
void ail_resume_3D_sample(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0)); s && !s->playing && s->wave.pcm_bytes) {
        if (s->channel < 0)
            s->channel = dx_alloc_audio_channel();
        if (s->channel >= 0) {
            s->remaining = s->loops;
            s->eos_fired = false;
            sample_play(*s);
        }
    }
    set_eax(c, 0);
}
void ail_end_3D_sample(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0)))
        sample_stop(*s);
    set_eax(c, 0);
}
void ail_3D_sample_status(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    if (s)
        sample_update(*s);
    set_eax(c, !s ? SMP_FREE : s->playing ? SMP_PLAYING : SMP_DONE);
}
void ail_register_3D_eos_callback(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0))) {
        s->eos_cb = arg(c, 1);
        s->eos_fired = false;
    }
    set_eax(c, 0);
}

// ---------------------------------------------------------------------------
// Miles timers.
// ---------------------------------------------------------------------------
AilTimer *timer_for(uint32_t handle) {
    for (auto &t : g_ail_timers)
        if (t.alive && t.handle == handle)
            return &t;
    return nullptr;
}

// AIL_register_timer(AILTIMERCB callback) returns a handle, or -1 when no
// timer slot is free. The callback is a guest address and is invoked with the
// user word.
void ail_register_timer(X86 *c) {
    set_eax(c, 0xffffffffu);
    if (!arg(c, 0))
        return;
    for (uint32_t i = 0; i < g_ail_timers.size(); ++i) {
        if (g_ail_timers[i].alive)
            continue;
        AilTimer &t = g_ail_timers[i];
        t = AilTimer{};
        t.alive = true;
        t.handle = i + 1;
        t.callback = arg(c, 0);
        set_eax(c, t.handle);
        return;
    }
}
void ail_set_timer_user(X86 *c) {
    if (AilTimer *t = timer_for(arg(c, 0)))
        t->user = arg(c, 1);
    set_eax(c, 0);
}
void ail_set_timer_frequency(X86 *c) {
    if (AilTimer *t = timer_for(arg(c, 0))) {
        t->frequency = std::clamp(arg(c, 1), 1u, 1000u);
        if (t->running)
            t->next_ns = os_monotonic_ns() + 1000000000ull / t->frequency;
    }
    set_eax(c, 0);
}
void ail_start_timer(X86 *c) {
    if (AilTimer *t = timer_for(arg(c, 0))) {
        t->running = true;
        t->next_ns = os_monotonic_ns() + 1000000000ull / std::max(1u, t->frequency);
    }
    set_eax(c, 0);
}
void ail_stop_timer(X86 *c) {
    if (AilTimer *t = timer_for(arg(c, 0)))
        t->running = false;
    set_eax(c, 0);
}
void ail_release_timer_handle(X86 *c) {
    if (AilTimer *t = timer_for(arg(c, 0)))
        *t = AilTimer{};
    set_eax(c, 0);
}

void ail_close_digital_driver(X86 *c) {
    uint32_t id = arg(c, 0);
    stream_close_driver(id);
    for (auto &s : g_samples)
        if (s.alive && s.driver == id) {
            sample_reset(s);
            s.alive = false;
        }
    auto it = g_drivers.find(id);
    if (it != g_drivers.end()) {
        driver_release(it->second);
        host_audio_delete_reverb(id);
        g_drivers.erase(it);
    }
    if (g_last_digital_driver == id)
        g_last_digital_driver = 0;
    set_eax(c, 0);
}

void ail_sample_playback_rate(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    set_eax(c, s ? (s->rate ? s->rate : s->wave.rate) : 0);
}

void ail_sample_volume_pan(X86 *c) {
    Sample *s = sample_for(arg(c, 0));
    if (arg(c, 1) && gm_valid(arg(c, 1), 4))
        wr32(arg(c, 1), float_to_bits(s ? s->volume01 : 0.0f));
    if (arg(c, 2) && gm_valid(arg(c, 2), 4))
        wr32(arg(c, 2), float_to_bits(s ? s->pan01 : 0.0f));
    set_eax(c, 0);
}

void ail_service_stream(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    if (Stream *s = stream_for(arg(c, 0)))
        stream_update(*s);
    set_eax(c, 0);
}

void ail_stream_loop_count(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    Stream *s = stream_for(arg(c, 0));
    set_eax(c, s ? s->loops : 0);
}

void ail_stream_sample_handle(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    Stream *s = stream_for(arg(c, 0));
    set_eax(c, s ? s->sample : 0);
}

void ail_set_listener_3D_position(X86 *c) {
    float x = bits_to_float(arg(c, 1)), y = bits_to_float(arg(c, 2)), z = bits_to_float(arg(c, 3));
    if (g_drivers.count(arg(c, 0))) {
        g_listener_pos3d[0] = x;
        g_listener_pos3d[1] = y;
        g_listener_pos3d[2] = z;
        sample_update_3d_mix();
    }
    set_eax(c, 0);
}

void ail_set_listener_3D_orientation(X86 *c) {
    if (g_drivers.count(arg(c, 0))) {
        for (int i = 0; i < 3; ++i) {
            g_listener_front3d[i] = bits_to_float(arg(c, 1 + i));
            g_listener_top3d[i] = bits_to_float(arg(c, 4 + i));
        }
        sample_update_3d_mix();
    }
    set_eax(c, 0);
}

void ail_set_sample_3D_position(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0))) {
        s->is_3d = true;
        s->pos3d[0] = bits_to_float(arg(c, 1));
        s->pos3d[1] = bits_to_float(arg(c, 2));
        s->pos3d[2] = bits_to_float(arg(c, 3));
        sample_apply_mix(*s);
    }
    set_eax(c, 0);
}

// AIL_set_sample_3D_distances(H3DSAMPLE, F32 max, F32 min, S32 apply).
void ail_set_sample_3D_distances(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0))) {
        s->max_d = bits_to_float(arg(c, 1));
        s->min_d = bits_to_float(arg(c, 2));
        sample_apply_mix(*s);
    }
    set_eax(c, 0);
}

void ail_set_sample_3D_orientation(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0))) {
        s->is_3d = true;
        for (int i = 0; i < 3; ++i) {
            s->front3d[i] = bits_to_float(arg(c, 1 + i));
            s->up3d[i] = bits_to_float(arg(c, 4 + i));
        }
        sample3d_normalize_or_keep(s->front3d);
        sample3d_normalize_or_keep(s->up3d);
        sample_apply_mix(*s);
    }
    set_eax(c, 0);
}

void ail_set_sample_3D_cone(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0))) {
        s->is_3d = true;
        float inner = bits_to_float(arg(c, 1));
        s->cone_inner = inner * (audio3d::PI_F_ / 360.0f);
        s->cone_outer = bits_to_float(arg(c, 2)) * (audio3d::PI_F_ / 360.0f);
        s->cone_outer_volume = bits_to_float(arg(c, 3));
        s->cone_enabled = inner < 360.0f && s->cone_outer_volume < 1.0f;
        sample_apply_mix(*s);
    }
    set_eax(c, 0);
}

void ail_set_sample_3D_velocity_vector(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0))) {
        float velocity[3] = {bits_to_float(arg(c, 1)), bits_to_float(arg(c, 2)),
                             bits_to_float(arg(c, 3))};
        if (velocity[0] != 0.0f || velocity[1] != 0.0f || velocity[2] != 0.0f) {
            LOGW("mss32: unsupported nonzero sample velocity (%g, %g, %g) for %08x", velocity[0],
                 velocity[1], velocity[2], s->handle);
            imports_unsupported(c);
            return;
        }
        s->is_3d = true;
        std::copy(std::begin(velocity), std::end(velocity), s->vel3d);
        sample_apply_mix(*s);
    }
    set_eax(c, 0);
}

void ail_set_3D_doppler_factor(X86 *c) {
    auto it = g_drivers.find(arg(c, 0));
    float factor = bits_to_float(arg(c, 1));
    if (it == g_drivers.end() || !std::isfinite(factor)) {
        LOGW("mss32: unsupported Doppler driver=%08x factor=%g", arg(c, 0), factor);
        imports_unsupported(c);
        return;
    }
    it->second.doppler = factor;
    set_eax(c, 0);
}

void ail_set_3D_rolloff_factor(X86 *c) {
    g_rolloff3d = bits_to_float(arg(c, 1));
    sample_update_3d_mix();
    set_eax(c, 0);
}

void ret0(X86 *c) {
    set_eax(c, 0);
}
void ret1(X86 *c) {
    set_eax(c, 1);
}
void ret_done(X86 *c) {
    set_eax(c, SMP_DONE);
}

// AIL_delay(intervals) waits in 1/60-second units.
void ail_delay(X86 *c) {
    guest_sleep_ms(arg(c, 0) * 1000 / 60);
    set_eax(c, 0);
}

// AIL_digital_handle_release/reacquire hand the wave device to another player
// (a Bink movie) and take it back. Host playback shares the device, so both
// succeed and voices keep their state.
void ail_digital_handle_toggle(X86 *c) {
    set_eax(c, 1);
}

// Miles 6's elliptical distances: front and back pairs. The mix models one
// pair, so the front distances are used.
void ail_set_3D_sample_float_distances(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0))) {
        s->max_d = bits_to_float(arg(c, 1));
        s->min_d = bits_to_float(arg(c, 2));
        sample_apply_mix(*s);
    }
    set_eax(c, 0);
}

// AIL_set_3D_sample_info(H3DSAMPLE, const AILSOUNDINFO *): format, data,
// length, rate, bits and channels of an image already in memory. PCM only.
void ail_set_3D_sample_info(X86 *c) {
    set_eax(c, 0);
    Sample *s = sample_for(arg(c, 0));
    uint32_t info = arg(c, 1);
    if (!s || !info || !gm_valid(info, 24))
        return;
    uint32_t format = rd32(info), data = rd32(info + 4), bytes = rd32(info + 8);
    uint32_t rate = rd32(info + 12), bits = rd32(info + 16), channels = rd32(info + 20);
    if (format != 1 || (bits != 8 && bits != 16) || (channels != 1 && channels != 2) || !rate) {
        LOGW("mss32: AIL_set_3D_sample_info format %u bits %u channels %u is unsupported", format,
             bits, channels);
        return;
    }
    sample_stop(*s);
    s->decoded.clear();
    bool ok = data && bytes && gm_valid(data, bytes);
    s->wave.pcm = ok ? data : 0;
    s->wave.pcm_bytes = ok ? bytes : 0;
    s->wave.rate = rate;
    s->wave.bits = (uint16_t)bits;
    s->wave.channels = (uint16_t)channels;
    set_eax(c, ok ? 1 : 0);
}

// AIL_set_sample_loop_block(HSAMPLE, start, end): loop a byte range of the
// image; end -1 is the end. Looping the whole image is what the mix does; a
// narrower range is not modelled and is logged.
void ail_set_sample_loop_block(X86 *c) {
    if (Sample *s = sample_for(arg(c, 0))) {
        int32_t start = (int32_t)arg(c, 1), end = (int32_t)arg(c, 2);
        if (start > 0 || (end >= 0 && (uint32_t)end < s->wave.pcm_bytes))
            log_once("mss32.loop_block",
                     "mss32: sample loop block %d..%d is not applied; the whole "
                     "sample loops",
                     start, end);
    }
    set_eax(c, 0);
}

void ail_set_stream_playback_rate(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    if (Stream *s = stream_for(arg(c, 0))) {
        if (Sample *h = sample_for(s->sample))
            h->rate = arg(c, 1);
        if (s->channel >= 0 && arg(c, 1))
            host_audio_set_frequency(s->channel, arg(c, 1));
    }
    set_eax(c, 0);
}

void ail_stream_playback_rate(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    Stream *s = stream_for(arg(c, 0));
    Sample *h = s ? sample_for(s->sample) : nullptr;
    set_eax(c, h && h->rate ? h->rate : s ? s->source.rate() : 0);
}

// AIL_stream_ms_position(HSTREAM, S32 *total_ms, S32 *current_ms): the track's
// length and how far the host has played into it, wrapped for a looping stream.
void ail_stream_ms_position(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    Stream *s = stream_for(arg(c, 0));
    uint32_t total_p = arg(c, 1), current_p = arg(c, 2);
    uint64_t total_ms = 0, current_ms = 0;
    if (s && s->source.rate()) {
        uint32_t rate = s->source.rate(), frame_bytes = s->source.channels() * 2;
        uint64_t total = s->source.total_frames();
        uint64_t played = s->playing || s->paused
                              ? s->base_frames + host_audio_played_bytes(s->channel) / frame_bytes
                              : s->start_frames;
        if (total)
            played %= total;
        total_ms = total * 1000 / rate;
        current_ms = played * 1000 / rate;
    }
    if (total_p && gm_valid(total_p, 4))
        wr32(total_p, (uint32_t)total_ms);
    if (current_p && gm_valid(current_p, 4))
        wr32(current_p, (uint32_t)current_ms);
    set_eax(c, 0);
}

// AIL_set_stream_ms_position(HSTREAM, S32 ms): a playing stream restarts there,
// a stopped one starts there next.
void ail_set_stream_ms_position(X86 *c) {
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    if (Stream *s = stream_for(arg(c, 0))) {
        uint64_t frames =
            (uint64_t)std::max<int32_t>((int32_t)arg(c, 1), 0) * s->source.rate() / 1000;
        uint64_t total = s->source.total_frames();
        s->start_frames = total ? std::min(frames, total) : frames;
        if (s->playing)
            stream_start(*s);
    }
    set_eax(c, 0);
}

#define AIL(name, bytes, fn) {"mss32.dll", "_AIL_" #name "@" #bytes, (bytes) / 4, fn}

const ImportShim g_mss32_shims[] = {
    AIL(startup, 0, ret1),
    AIL(shutdown, 0, ret0),
    // AIL_set_redist_directory tells Miles where to find its own runtime
    // modules. The shim is the runtime, so the path is irrelevant; the engine
    // ignores the result and calls AIL_startup next.
    AIL(set_redist_directory, 4, ret1),
    AIL(lock, 0, ail_lock),
    AIL(unlock, 0, ail_unlock),
    AIL(stop_sample, 4, ail_stop_sample),
    AIL(resume_sample, 4, ail_resume_sample),
    AIL(set_preference, 8, ail_preference),
    AIL(get_preference, 4, ail_get_preference),
    AIL(waveOutOpen, 16, ail_wave_open),
    AIL(waveOutClose, 4, ail_wave_close),
    AIL(open_digital_driver, 16, ail_open_digital_driver),
    AIL(digital_configuration, 16, ail_configuration),
    AIL(set_digital_master_volume, 8, ail_master_volume),
    AIL(set_named_sample_file, 20, ail_set_named_sample_file),
    AIL(mem_free_lock, 4, ail_mem_free_lock),
    AIL(mem_alloc_lock, 4, ail_mem_alloc_lock),
    AIL(file_read, 8, ail_file_read),
    AIL(file_size, 4, ail_file_size),
    AIL(ms_count, 0, ail_ms_count),
    AIL(open_input, 4, ail_open_input),
    AIL(close_input, 4, ret0),
    AIL(set_input_state, 8, ret0),
    AIL(sample_volume, 4, ail_sample_volume),
    AIL(sample_pan, 4, ail_sample_pan),
    AIL(digital_master_volume, 4, ail_digital_master_volume),
    AIL(set_sample_type, 12, ail_set_sample_type),
    AIL(set_sample_address, 12, ail_set_sample_address),
    AIL(set_sample_user_data, 12, ail_set_sample_user_data),
    AIL(sample_user_data, 8, ail_sample_user_data),
    AIL(set_3D_user_data, 12, ail_set_sample_user_data),
    AIL(3D_user_data, 8, ail_sample_user_data),
    AIL(set_3D_sample_obstruction, 8, ail_set_3D_sample_obstruction),
    AIL(3D_sample_obstruction, 4, ail_3D_sample_obstruction),
    AIL(3D_sample_occlusion, 4, ail_3D_sample_occlusion),
    AIL(3D_sample_playback_rate, 4, ail_3D_sample_playback_rate),
    AIL(3D_position, 16, ail_3D_position),
    AIL(3D_orientation, 28, ail_3D_orientation),
    AIL(3D_velocity, 16, ail_3D_velocity),
    AIL(set_3D_velocity_vector, 16, ail_set_3D_velocity_vector),
    AIL(last_error, 0, ail_last_error),
    AIL(file_type, 8, ail_file_type),
    AIL(WAV_info, 8, ail_wav_info),
    AIL(allocate_sample_handle, 4, ail_allocate_sample_handle),
    AIL(release_sample_handle, 4, ail_release_sample_handle),
    AIL(init_sample, 4, ail_init_sample),
    AIL(set_sample_file, 12, ail_set_sample_file),
    AIL(start_sample, 4, ail_start_sample),
    AIL(end_sample, 4, ail_end_sample),
    AIL(sample_status, 4, ail_sample_status),
    AIL(set_sample_volume, 8, ail_set_sample_volume),
    AIL(set_sample_pan, 8, ail_set_sample_pan),
    AIL(set_sample_loop_count, 8, ail_set_sample_loop_count),
    AIL(sample_loop_count, 4, ail_sample_loop_count),
    AIL(set_sample_playback_rate, 8, ail_set_sample_playback_rate),
    AIL(set_sample_volume_levels, 12, ail_set_sample_volume_levels),
    AIL(set_sample_volume_pan, 12, ail_set_sample_volume_pan),
    AIL(set_sample_reverb, 16, ret0),
    AIL(register_EOS_callback, 8, ail_register_eos_callback),
    AIL(register_stream_callback, 8, ret0),
    AIL(register_timer, 4, ail_register_timer),
    AIL(release_timer_handle, 4, ail_release_timer_handle),
    AIL(set_timer_user, 8, ail_set_timer_user),
    AIL(set_timer_frequency, 8, ail_set_timer_frequency),
    AIL(start_timer, 4, ail_start_timer),
    AIL(stop_timer, 4, ail_stop_timer),
    AIL(open_stream, 12, ail_open_stream),
    AIL(start_stream, 4, ail_start_stream),
    AIL(close_stream, 4, ail_close_stream),
    AIL(stream_status, 4, ail_stream_status),
    AIL(set_stream_volume, 8, ail_set_stream_volume),
    AIL(stream_volume, 4, ail_stream_volume),
    AIL(set_stream_loop_count, 8, ail_set_stream_loop_count),
    AIL(pause_stream, 8, ail_pause_stream),
    AIL(set_stream_volume_levels, 12, ail_set_stream_volume_levels),
    AIL(enumerate_3D_providers, 12, ail_enumerate_3D_providers),
    AIL(open_3D_provider, 4, ail_open_3D_provider),
    AIL(close_3D_provider, 4, ret0),
    AIL(open_3D_listener, 4, ail_open_3D_listener),
    AIL(close_3D_listener, 4, ret0),
    AIL(3D_speaker_type, 4, ail_3D_speaker_type),
    AIL(set_3D_speaker_type, 8, ail_set_3D_speaker_type),
    AIL(3D_room_type, 4, ail_3D_room_type),
    AIL(set_3D_distance_factor, 8, ail_set_3D_distance_factor),
    AIL(set_3D_provider_preference, 12, ret0),
    AIL(3D_provider_attribute, 12, ail_3D_provider_attribute),
    AIL(allocate_3D_sample_handle, 4, ail_allocate_3D_sample_handle),
    AIL(release_3D_sample_handle, 4, ail_release_3D_sample_handle),
    AIL(set_3D_sample_file, 8, ail_set_3D_sample_file),
    AIL(start_3D_sample, 4, ail_start_3D_sample),
    AIL(stop_3D_sample, 4, ail_stop_3D_sample),
    AIL(resume_3D_sample, 4, ail_resume_3D_sample),
    AIL(end_3D_sample, 4, ail_end_3D_sample),
    AIL(3D_sample_status, 4, ail_3D_sample_status),
    AIL(set_3D_sample_volume, 8, ail_set_3D_sample_volume),
    AIL(set_3D_sample_loop_count, 8, ail_set_3D_sample_loop_count),
    AIL(set_3D_sample_distances, 12, ail_set_3D_sample_distances),
    AIL(set_3D_sample_playback_rate, 8, ail_set_3D_sample_playback_rate),
    AIL(set_3D_sample_effects_level, 8, ail_set_3D_sample_effects_level),
    AIL(set_3D_sample_occlusion, 8, ail_set_3D_sample_occlusion),
    AIL(set_3D_position, 16, ail_set_3D_position),
    AIL(set_3D_orientation, 28, ail_set_3D_orientation),
    AIL(register_3D_EOS_callback, 8, ail_register_3D_eos_callback),
    AIL(3D_update_position, 8, ret0),
    AIL(mem_use_malloc, 4, ail_mem_use_malloc),
    AIL(delay, 4, ail_delay),
    AIL(digital_handle_release, 4, ail_digital_handle_toggle),
    AIL(digital_handle_reacquire, 4, ail_digital_handle_toggle),
    AIL(set_3D_sample_float_distances, 20, ail_set_3D_sample_float_distances),
    AIL(set_3D_sample_info, 8, ail_set_3D_sample_info),
    AIL(set_sample_loop_block, 12, ail_set_sample_loop_block),
    AIL(set_3D_sample_loop_block, 12, ail_set_sample_loop_block),
    AIL(set_stream_playback_rate, 8, ail_set_stream_playback_rate),
    AIL(stream_playback_rate, 4, ail_stream_playback_rate),
    AIL(mem_use_free, 4, ail_mem_use_free),
    AIL(close_digital_driver, 4, ail_close_digital_driver),
    AIL(get_DirectSound_info, 12, ail_get_dsound_info),
    AIL(sample_ms_position, 12, imports_unsupported),
    AIL(sample_playback_rate, 4, ail_sample_playback_rate),
    AIL(sample_position, 4, imports_unsupported),
    AIL(sample_reverb_levels, 12, ail_sample_reverb_levels),
    AIL(sample_volume_pan, 12, ail_sample_volume_pan),
    AIL(service_stream, 8, ail_service_stream),
    AIL(set_3D_doppler_factor, 8, ail_set_3D_doppler_factor),
    AIL(set_3D_rolloff_factor, 8, ail_set_3D_rolloff_factor),
    AIL(set_digital_master_reverb, 16, ail_set_digital_master_reverb),
    AIL(digital_master_reverb, 16, ail_digital_master_reverb),
    AIL(set_digital_master_reverb_levels, 12, ail_set_digital_master_reverb_levels),
    AIL(digital_master_reverb_levels, 12, ail_digital_master_reverb_levels),
    AIL(set_listener_3D_orientation, 28, ail_set_listener_3D_orientation),
    AIL(set_listener_3D_position, 16, ail_set_listener_3D_position),
    AIL(set_room_type, 8, ail_set_room_type),
    AIL(room_type, 4, ail_room_type),
    AIL(set_sample_3D_cone, 16, ail_set_sample_3D_cone),
    AIL(set_sample_3D_distances, 16, ail_set_sample_3D_distances),
    AIL(set_sample_3D_orientation, 28, ail_set_sample_3D_orientation),
    AIL(set_sample_3D_position, 16, ail_set_sample_3D_position),
    AIL(set_sample_3D_velocity_vector, 16, ail_set_sample_3D_velocity_vector),
    AIL(set_sample_position, 8, imports_unsupported),
    AIL(set_sample_reverb_levels, 12, ail_set_sample_reverb_levels),
    AIL(set_stream_ms_position, 8, ail_set_stream_ms_position),
    AIL(stream_loop_count, 4, ail_stream_loop_count),
    AIL(stream_ms_position, 12, ail_stream_ms_position),
    AIL(stream_sample_handle, 4, ail_stream_sample_handle),
};

} // namespace

void mss32_frame_pump(X86 *c) {
    if (!c)
        return;
    for (auto &s : g_samples) {
        if (!s.alive || s.stream >= 0)
            continue;
        if (sample_update(s) && s.eos_cb && !s.eos_fired) {
            // Mark before the call: the callback may stop or release the
            // sample, and re-entering the shim must not fire it twice.
            s.eos_fired = true;
            uint32_t cb = s.eos_cb, handle = s.handle;
            guest_call(c, cb, handle);
        }
    }
    // Miles timers, on the same seam. Each pass is bounded so a callback that
    // has slipped behind cannot run away inside one frame; the bound is per
    // timer, and a released timer is re-read rather than held as a reference.
    uint64_t now = os_monotonic_ns();
    for (size_t i = 0; i < g_ail_timers.size(); ++i) {
        for (int guard = 0; guard < 8; ++guard) {
            AilTimer &t = g_ail_timers[i];
            if (!t.alive || !t.running || now < t.next_ns)
                break;
            uint64_t period = 1000000000ull / std::max(1u, t.frequency);
            t.next_ns += period;
            uint32_t cb = t.callback, user = t.user;
            guest_call(c, cb, user);
            now = os_monotonic_ns();
        }
    }
}

// Drop everything that names the guest arena and the host channels before
// dx_reset discards them. The provider name block and the sample WAVE images
// are guest heap allocations; the digital drivers and channels are host state.
void mss32_reset() {
    mss32_shutdown();
    std::lock_guard<std::mutex> lock(g_stream_mutex);
    for (auto &s : g_streams)
        stream_release(s);
    for (auto &s : g_samples)
        if (s.alive && s.channel >= 0)
            host_audio_stop(s.channel);
    g_samples.assign(g_samples.size(), Sample{});
    g_ail_timers.assign(g_ail_timers.size(), AilTimer{});
    for (auto &[id, driver] : g_drivers)
        host_audio_delete_reverb(id);
    g_drivers.clear();
    g_malloc_cb = g_free_cb = 0;
    g_last_digital_driver = 0;
    g_provider_names3d = 0;
    g_listener_pos3d[0] = g_listener_pos3d[1] = g_listener_pos3d[2] = 0.0f;
    g_listener_front3d[0] = 0.0f;
    g_listener_front3d[1] = 0.0f;
    g_listener_front3d[2] = 1.0f;
    g_listener_top3d[0] = 0.0f;
    g_listener_top3d[1] = 1.0f;
    g_listener_top3d[2] = 0.0f;
    g_rolloff3d = 1.0f;
    g_stream_host_shutdown = false;
}

void mss32_shutdown() {
    OsThread *thread = nullptr;
    {
        std::lock_guard<std::mutex> lock(g_stream_mutex);
        g_stream_host_shutdown = true;
        g_stream_worker_stop = true;
        g_stream_command = true;
        thread = g_stream_thread;
        g_stream_thread = nullptr;
        for (auto &s : g_streams) {
            if (!s.alive)
                continue;
            stream_report_refills(s);
            s.playing = false;
        }
        g_stream_condition.notify_one();
    }
    if (thread)
        os_thread_join(thread);
}

void mss32_register() {
    static bool done = false;
    if (done)
        return;
    done = true;
    imports_register(g_mss32_shims, std::size(g_mss32_shims));
}
