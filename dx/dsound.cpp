// dsound.cpp - DirectSound: the device, secondary buffers, the 3D buffer and
// listener interfaces, and notification positions.
//
// A secondary buffer owns guest memory the game locks and fills with PCM. Play
// forwards that memory, the format and the current volume and pan to
// host_audio_play; Task 7 mixes it. A buffer created with DSBCAPS_CTRL3D also
// runs the 3D distance attenuation and stereo panning from Wine's software
// DSOUND_Calc3DBuffer, recomputed at Play and whenever a position, listener or
// other 3D parameter changes; the formula and the reference are documented
// above calc_3d.
//
// Static cross-referencing of the EXE shows IID_IDirectSound3DBuffer,
// IID_IDirectSound3DListener and IID_IDirectSoundNotify are all referenced
// from code, so all three are implemented rather than refused.
#include "com.h"
#include "dx.h"
#include "audio3d.h"
#include "host_api.h"
#include "../runtime/memory.h"
#include "../runtime/win32.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <time.h>
#include <iterator>
#include "../platform/os.h"

#define IID_BYTES(a, b, c, d0, d1, d2, d3, d4, d5, d6, d7)                                         \
    {(uint8_t)((a) & 0xff),                                                                        \
     (uint8_t)(((a) >> 8) & 0xff),                                                                 \
     (uint8_t)(((a) >> 16) & 0xff),                                                                \
     (uint8_t)(((a) >> 24) & 0xff),                                                                \
     (uint8_t)((b) & 0xff),                                                                        \
     (uint8_t)(((b) >> 8) & 0xff),                                                                 \
     (uint8_t)((c) & 0xff),                                                                        \
     (uint8_t)(((c) >> 8) & 0xff),                                                                 \
     d0,                                                                                           \
     d1,                                                                                           \
     d2,                                                                                           \
     d3,                                                                                           \
     d4,                                                                                           \
     d5,                                                                                           \
     d6,                                                                                           \
     d7}

// Stable identity of the bridge's software playback device, distinct from the
// NULL-GUID primary-driver alias. This identifies our device, not host hardware.
static const uint8_t GUID_RecompPlayback_[16] =
    IID_BYTES(0xA802C861, 0xE478, 0x4A21, 0x91, 0xC8, 0x75, 0xD0, 0x04, 0x62, 0x58, 0x36);

static const uint8_t IID_IDirectSound_[16] =
    IID_BYTES(0x279AFA83, 0x4981, 0x11CE, 0xA5, 0x21, 0x00, 0x20, 0xAF, 0x0B, 0xE5, 0x60);
static const uint8_t IID_IDirectSound8_[16] =
    IID_BYTES(0xC50A7E93, 0xF395, 0x4834, 0x9E, 0xF6, 0x7F, 0xA9, 0x9D, 0xE5, 0x09, 0xE6);
static const uint8_t IID_IDirectSoundBuffer_[16] =
    IID_BYTES(0x279AFA85, 0x4981, 0x11CE, 0xA5, 0x21, 0x00, 0x20, 0xAF, 0x0B, 0xE5, 0x60);
static const uint8_t IID_IDirectSoundBuffer8_[16] =
    IID_BYTES(0x6825A449, 0x7524, 0x4D82, 0x92, 0x0F, 0x50, 0xE3, 0x6A, 0xB3, 0xAB, 0x1E);
static const uint8_t IID_IDirectSound3DListener_[16] =
    IID_BYTES(0x279AFA84, 0x4981, 0x11CE, 0xA5, 0x21, 0x00, 0x20, 0xAF, 0x0B, 0xE5, 0x60);
static const uint8_t IID_IDirectSound3DBuffer_[16] =
    IID_BYTES(0x279AFA86, 0x4981, 0x11CE, 0xA5, 0x21, 0x00, 0x20, 0xAF, 0x0B, 0xE5, 0x60);
static const uint8_t IID_IDirectSoundNotify_[16] =
    IID_BYTES(0xB0210783, 0x89CD, 0x11D0, 0xAF, 0x08, 0x00, 0xA0, 0xC9, 0x25, 0xCD, 0x16);

namespace {

// DuplicateSoundBuffer shares the original's sample data, so the memory
// outlives whichever buffer is released first. A reference count keyed by the
// guest address is the smallest thing that gets that right; releasing the
// original while a duplicate still plays must not free the samples.
std::vector<std::pair<uint32_t, uint32_t>> &pcm_refs() {
    static auto *v = new std::vector<std::pair<uint32_t, uint32_t>>();
    return *v;
}
void pcm_retain(uint32_t addr) {
    if (!addr)
        return;
    for (auto &e : pcm_refs())
        if (e.first == addr) {
            ++e.second;
            return;
        }
    pcm_refs().push_back({addr, 1});
}
void pcm_release(uint32_t addr) {
    if (!addr)
        return;
    for (size_t i = 0; i < pcm_refs().size(); ++i) {
        if (pcm_refs()[i].first != addr)
            continue;
        if (--pcm_refs()[i].second == 0) {
            heap_free(addr);
            pcm_refs().erase(pcm_refs().begin() + (ptrdiff_t)i);
        }
        return;
    }
    // Not tracked: nothing allocated it here, so nothing frees it.
}

ComObj *this_dsound(X86 *c) {
    ComObj *o = com_this_arg(c);
    return (o && o->kind == K_DSOUND) ? o : nullptr;
}
ComObj *this_buffer(X86 *c) {
    ComObj *o = com_this_arg(c);
    return (o && o->kind == K_DSBUFFER) ? o : nullptr;
}

// ---------------------------------------------------------------------------
// Notification positions.
//
// The game streams music into a looping secondary buffer and does it the way
// DirectSound intends: it asks IDirectSoundNotify for an event at offset 0 and
// one at the half-way point, plays the buffer looping, and parks a worker
// thread in WaitForMultipleObjects on those two events. Every time one fires
// it refills the half the play cursor has just left. (0056ec50 creates the
// thread and the events, 0056efe0 registers the two positions and calls Play,
// 0056eb70 is the wait loop, 0056f090/00576390 are the refill and the
// Lock/copy/Unlock it ends in.)
//
// So a shim that records the positions and never signals them does not merely
// lose a notification: the worker never wakes, the second half of the buffer
// is never written, and the host loops the one half that was filled before
// Play for the rest of the run. That is what the first live run heard.
//
// Nothing here runs on a host thread. The positions are serviced whenever a
// guest thread enters this module or QSWaveMixPump, or yields at a scheduler
// checkpoint, which covers loading when no frames run. The signal itself goes
// through guest_event_signal_from_host, which queues it for the scheduler.
// ---------------------------------------------------------------------------
const uint32_t DSBPN_OFFSETSTOP = 0xffffffffu;

struct NotifyPos {
    uint32_t offset = 0;
    uint32_t event = 0;
};

struct NotifyRecord {
    uint32_t obj_id = 0;
    std::vector<NotifyPos> pos;
    uint32_t last = 0;     // where the play cursor was last seen
    bool tracking = false; // false until the first service after a Play
};

std::vector<NotifyRecord> &notifies() {
    static auto *v = new std::vector<NotifyRecord>();
    return *v;
}

NotifyRecord *notify_for(uint32_t id, bool create = false) {
    for (auto &n : notifies())
        if (n.obj_id == id)
            return &n;
    if (!create)
        return nullptr;
    notifies().push_back(NotifyRecord{});
    notifies().back().obj_id = id;
    return &notifies().back();
}

// ---------------------------------------------------------------------------
// Streaming into a looping buffer.
//
// This is how the game actually plays its music and speech, and the Wine trace
// of the original is unambiguous about it. Buffer 0120FDF8: 32768 bytes,
// 22050 Hz stereo 16-bit, DSBCAPS_GETCURRENTPOSITION2 | CTRLVOLUME | CTRLPAN |
// CTRLFREQUENCY, `Play(0, 0, DSBPLAY_LOOPING)` while it is still empty. Then a
// dedicated thread polls GetCurrentPosition every ten milliseconds and, every
// forty, locks three or four kilobytes at its own running write offset - 0,
// 3584, 7056, 10640, ... wrapping at 32768 - and unlocks it. It stays about a
// lap ahead of the play cursor. No notification positions are ever registered
// on this path; the poll is the whole clock.
//
// So the first lap really is silence, in the original as much as here, and the
// zero-peak first buffer of the first live run was the ring as Play found it.
// What was wrong was afterwards: re-submitting the whole ring on each Unlock
// restarts the voice twenty-five times a second, which is what "garbled"
// sounds like.
//
// The fix is to stop pretending the ring is a loop once the guest starts
// writing into it. Ring order from the play cursor *is* play order, so the
// rest of the current lap is re-issued as a one-shot and every region the
// guest unlocks afterwards is appended behind it with host_audio_queue. The
// join is sample-accurate and the voice never restarts.
//
// A looping buffer nobody writes into is left alone: it is a loop, and the
// host keeps looping it.
// ---------------------------------------------------------------------------
struct StreamRecord {
    uint32_t obj_id = 0;
    bool active = false;   // converted, and being fed by Unlock
    bool refused = false;  // this host cannot stream; stay on re-submission
    bool fed_once = false; // ring_end names a real feed, not the seed
    uint32_t ring_end = 0; // ring offset just past the last byte fed
};

std::vector<StreamRecord> &streams() {
    static auto *v = new std::vector<StreamRecord>();
    return *v;
}

StreamRecord *stream_for(uint32_t id, bool create = false) {
    for (auto &r : streams())
        if (r.obj_id == id)
            return &r;
    if (!create)
        return nullptr;
    streams().push_back(StreamRecord{});
    streams().back().obj_id = id;
    return &streams().back();
}

void stream_drop(uint32_t id) {
    for (size_t i = 0; i < streams().size(); ++i) {
        if (streams()[i].obj_id != id)
            continue;
        streams().erase(streams().begin() + (ptrdiff_t)i);
        return;
    }
}

// A converted stream ends whenever the sound it belongs to does. The record is
// kept - the buffer may be played again - but it starts from nothing.
void stream_reset(uint32_t id) {
    if (StreamRecord *r = stream_for(id)) {
        r->active = false;
        r->fed_once = false;
        r->ring_end = 0;
    }
}

// Where a converted stream's play cursor is.
//
// The host owns this now. host_audio_stream resumes the ring at the offset the
// loop had reached and reports it, and host_audio_played_bytes counts on from
// there at the sample rate, never going backwards, across a re-schedule,
// across a refill and across an underrun. Reducing that modulo the ring is the
// whole of it.
//
// It has to be a count of what has been played and not of what is in flight.
// The guest keeps about a lap of audio in flight on purpose, so a cursor
// derived from the outstanding bytes sits a fixed distance ahead of the
// guest's own write offset for ever - and that distance is exactly what the
// video player's refill gate measures (0057df10 returns the bytes from its
// write offset forward to the cursor, 0057dc80 compares that to the next
// chunk's length). Freeze it and the guest stops writing, which stops the
// stream, which freezes it further: the movie waits on a number that will not
// move, with nothing to show that anything is wrong.
uint32_t stream_position(const ComObj *b) {
    return b->buf_bytes ? host_audio_played_bytes(b->channel) % b->buf_bytes : 0;
}

// Where the buffer's play cursor is now, in bytes from the start of the
// buffer. For a buffer that is not a converted stream the host models it and
// it moves whether or not anything asks; a host that does not track playback
// reports 0, which reads as "still at the start" and simply never crosses a
// position.
uint32_t live_position(const ComObj *b) {
    if (!b->playing || b->channel < 0 || !b->buf_bytes)
        return b->play_cursor;
    for (const auto &e : streams())
        if (e.obj_id == b->id && e.active)
            return stream_position(b);
    return host_audio_position(b->channel) % b->buf_bytes;
}

// Did the cursor pass `o` on its way from `last` to `pos`? The interval is
// half-open the way DirectSound's own notification check is: closed at the
// near end and open at the far end, matching Wine's DSOUND_CheckEvent, which
// signals every offset in [old, new) as the buffer is mixed. That near-end
// inclusion is what makes the notification at the start offset fire on the
// first move off it, which is the event the game's streaming worker waits for
// before refilling the quarter ahead of the cursor. With the interval open at
// the near end instead, offset 0 is only ever reached by wrapping, and every
// refill is then due at the very instant its audio is played.
// A looping buffer wraps, and offset 0 is reached on the way past, which is
// why the wrapped case is written out rather than folded into the other.
bool cursor_crossed(uint32_t last, uint32_t pos, uint32_t o) {
    if (pos == last)
        return false;
    if (pos > last)
        return o >= last && o < pos;
    return o >= last || o < pos;
}

void signal_stop_positions(const ComObj *b) {
    NotifyRecord *n = notify_for(b->id);
    if (!n)
        return;
    for (const NotifyPos &p : n->pos)
        if (p.offset == DSBPN_OFFSETSTOP && p.event)
            guest_event_signal_from_host(p.event);
    n->tracking = false;
}

// Walks every buffer that registered positions and signals the ones the play
// cursor has reached since the last look. Cheap: the list holds only buffers
// that asked, which for this game is one per streaming voice.
void service_notifications() {
    for (NotifyRecord &n : notifies()) {
        if (n.pos.empty())
            continue;
        ComObj *b = com_get(n.obj_id);
        if (!b || b->kind != K_DSBUFFER)
            continue;

        // A one-shot that has run out stops at the end of its data; that is a
        // stop, and DSBPN_OFFSETSTOP is what reports it.
        if (b->playing && !b->looping && b->channel >= 0 && !host_audio_is_playing(b->channel)) {
            b->playing = false;
            b->play_cursor = b->buf_bytes;
            signal_stop_positions(b);
            continue;
        }
        if (!b->playing || b->channel < 0 || !b->buf_bytes)
            continue;

        uint32_t pos = live_position(b);
        if (!n.tracking) {
            n.last = pos;
            n.tracking = true;
            continue;
        }
        if (pos == n.last)
            continue;
        for (const NotifyPos &p : n.pos) {
            if (p.offset == DSBPN_OFFSETSTOP || !p.event)
                continue;
            if (p.offset >= b->buf_bytes)
                continue;
            if (cursor_crossed(n.last, pos, p.offset))
                guest_event_signal_from_host(p.event);
        }
        n.last = pos;
        b->play_cursor = pos;
    }
}

// The listener is a view on the DirectSound object, so its state lives there.
float g_listener[12] = {0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0};
float g_distance_factor = 1.0f, g_doppler_factor = 1.0f, g_rolloff_factor = 1.0f;

// ---------------------------------------------------------------------------
// 3D positional audio: distance attenuation and stereo panning.
//
// Reference. The algorithm below is Wine's software DirectSound 3D path,
// dlls/dsound/sound3d.c `DSOUND_Calc3DBuffer` (wine-mirror/wine, master),
// cross-checked against Microsoft's documented 3D attenuation formula and the
// DS3DBUFFER/DS3DLISTENER defaults in include/dsound.h. It is Win32 x86, runs
// in software with DS3DMODE_NORMAL and no HRTF, which is exactly the case the
// game uses. This is a formula and constant reference, not copied code: the
// shape (clamp, rolloff, inverse distance) and the pan/cone/Doppler layout are
// Wine's, the arithmetic here is written for this shim.
//
// Distance attenuation (Wine sound3d.c 216-237). Microsoft's DirectX reference
// states the same min/max semantics - no gain increase inside MinDistance, and
// no further attenuation past MaxDistance - but the exact closed form below is
// Wine's implementation; the MSDN formula page itself was not reachable to
// quote directly.
//
//   d = |buffer - listener|                         (DS3DMODE_NORMAL)
//   d = |buffer|                                    (DS3DMODE_HEADRELATIVE)
//   if (d > max)  d = max                           (unless the buffer was
//                                                    created with the
//                                                    MUTE3DATMAXDISTANCE
//                                                    flag, which silences it)
//   if (d < min)  d = min
//   adjusted = min + (d - min) * rolloffFactor
//   gain = min / adjusted
//
// i.e. inverse-distance 1/d beyond MinDistance, MinDistance inside it, and
// constant (not silent) beyond MaxDistance unless the mute flag is set. Note
// that RolloffFactor scales the distance *before* the 1/d, so it is not the
// exponent; the guest leaves it at the 1.0 default. Wine ignores the listener's
// flDistanceFactor for attenuation entirely - it only appears in its Doppler
// term - so neither do we.
//
// Panning (Wine sound3d.c 286-307, 344-361; stereo speaker table in dsound.c
// 1067-1074: speaker_angles = {-pi/2, +pi/2}, speaker_num = {left, right}):
//
//   left = orientFront x orientTop            (DirectX's left-handed x
//                                              already makes this point left)
//   angle = angleBetween(left, dir)
//   if angleBetween(front, dir) > pi/2: angle = -angle   (source is behind)
//   angle -= pi/2
//   if angle < -pi: angle += 2*pi
//
// then an equal-power crossfade over the two speakers, sqrt(1-a) and sqrt(a),
// where a ramps from 0 at the left speaker to 1 at the right. A source dead
// ahead is therefore sqrt(0.5) in each channel, not unity. Position and
// listener are left-handed world coordinates and the cross product is taken
// in that convention, so no axis flip is applied.
//
// The host mixer's pan is a one-sided attenuation, not a position, so the two
// speaker gains are folded into a host volume plus a host pan. The louder
// speaker becomes the channel volume and the quieter one the pan ratio; the
// host then reproduces both gains exactly. See host/audio.h.
//
// DIVERGENCE(original): Wine converts its millibel volume with
// pow(2, lVolume/600) while the host mixer uses 10^(mb/2000). The two agree to
// about 0.3% and the second is the exact decibel definition, so the host form
// is kept. Also not reproduced: the cone fields, Doppler/velocity, and the
// flDistanceFactor influence on velocity. The guest never uses them; each logs
// once when it is set (see B3D_SetCone*/B3D_SetVelocity/SetDopplerFactor) so a
// future title cannot silently lose them.
// ---------------------------------------------------------------------------
enum { DS3DMODE_NORMAL = 0, DS3DMODE_HEADRELATIVE = 1, DS3DMODE_DISABLE = 2 };
enum { DS3D_IMMEDIATE = 0, DS3D_DEFERRED = 1 };
static const uint32_t DSBCAPS_MUTE3DATMAXDISTANCE_ = 0x00020000u;

// The vector maths and speaker mix live in audio3d.h so the Miles software
// provider places a source the same way DirectSound3D does.
using namespace audio3d;

// The host volume and pan for one buffer right now. For a non-3D buffer, or a
// 3D buffer whose mode is DS3DMODE_DISABLE, that is simply its own SetVolume
// and SetPan values, which is what DirectSound does.
void calc_3d(const ComObj *b, int32_t *volume_mb, int32_t *pan_mb) {
    *volume_mb = b->volume;
    *pan_mb = b->pan;
    if (!b->is_3d || b->mode3d == DS3DMODE_DISABLE)
        return;

    const V3 buf = {b->pos3d[0], b->pos3d[1], b->pos3d[2]};
    const V3 front = {g_listener[3], g_listener[4], g_listener[5]};
    const V3 top = {g_listener[6], g_listener[7], g_listener[8]};
    V3 dir;
    if (b->mode3d == DS3DMODE_HEADRELATIVE)
        dir = buf; // already relative to the listener
    else
        dir = {buf.x - g_listener[0], buf.y - g_listener[1], buf.z - g_listener[2]};
    float dist = v3_len(dir);

    if (dist > b->max3d) {
        if (b->buf_flags & DSBCAPS_MUTE3DATMAXDISTANCE_) {
            *volume_mb = DSBVOLUME_MIN;
            *pan_mb = 0;
            return;
        }
        dist = b->max3d;
    }
    if (dist < b->min3d)
        dist = b->min3d;

    const float adjusted = b->min3d + (dist - b->min3d) * g_rolloff_factor;
    // A zero or negative MinDistance would divide by zero. DirectSound requires
    // it to be positive; keep full volume rather than propagate a NaN.
    const float dist_gain = (b->min3d > 0.0f && adjusted > 0.0f) ? b->min3d / adjusted : 1.0f;
    const float ingain = powf(10.0f, (float)b->volume / 2000.0f) * dist_gain;

    float angle = 0.0f;
    if (dist != 0.0f) {
        const V3 left = v3_cross(front, top);
        angle = v3_angle(left, dir);
        if (v3_angle(front, dir) > PI_F_ / 2.0f)
            angle = -angle;
        angle -= PI_F_ / 2.0f;
        if (angle < -PI_F_)
            angle += 2.0f * PI_F_;
    }
    float lg = 1.0f, rg = 1.0f;
    stereo_gains(angle, &lg, &rg);
    const float l = ingain * lg, r = ingain * rg;
    const float loud = l > r ? l : r;
    if (loud <= 0.0f) {
        *volume_mb = DSBVOLUME_MIN;
        *pan_mb = 0;
        return;
    }

    int32_t vm = (int32_t)lroundf(2000.0f * log10f(loud));
    if (vm > DSBVOLUME_MAX)
        vm = DSBVOLUME_MAX;
    if (vm < DSBVOLUME_MIN)
        vm = DSBVOLUME_MIN;
    *volume_mb = vm;

    float pan;
    if (l > 0.0f && r > 0.0f)
        pan = 2000.0f * log10f(r / l);
    else if (r > l)
        pan = (float)DSBPAN_RIGHT;
    else
        pan = (float)DSBPAN_LEFT;
    int32_t pm = (int32_t)lroundf(pan);
    if (pm > DSBPAN_RIGHT)
        pm = DSBPAN_RIGHT;
    if (pm < DSBPAN_LEFT)
        pm = DSBPAN_LEFT;
    *pan_mb = pm;
}

// Push a buffer's current 3D mix to its host channel. Non-3D and disabled
// buffers fall through to their own volume and pan.
void update_channel_mix(ComObj *b) {
    if (!b || b->channel < 0)
        return;
    int32_t volume_mb = b->volume, pan_mb = b->pan;
    calc_3d(b, &volume_mb, &pan_mb);
    host_audio_set_volume(b->channel, volume_mb);
    host_audio_set_pan(b->channel, pan_mb);
}

// The listener is shared by every 3D buffer, so a listener change with
// DS3D_IMMEDIATE - or a CommitDeferredSettings - recalcs all of them, exactly
// as Wine's DSOUND_ChangeListener walks the device buffer list. Stopped
// buffers need nothing: Play computes from the stored state.
void update_all_3d() {
    for (ComObj &o : com_objects()) {
        if (o.alive && o.kind == K_DSBUFFER && o.is_3d)
            update_channel_mix(&o);
    }
}

void read_wave_format(ComObj *b, uint32_t wfx) {
    if (!wfx || !gm_valid(wfx, 16))
        return;
    uint16_t tag = rd16(wfx + WFX_OFF_wFormatTag);
    // WAVEFORMATEXTENSIBLE (0xFFFE) names its real format in a SubFormat
    // GUID whose first four bytes are the old tag: 1 for PCM.
    if (tag == 0xfffe && gm_valid(wfx, 40) && rd16(wfx + 16) >= 22 &&
        rd32(wfx + 24) == WAVE_FORMAT_PCM && rd16(wfx + 28) == 0 && rd16(wfx + 30) == 0x10)
        tag = WAVE_FORMAT_PCM;
    if (tag != WAVE_FORMAT_PCM) {
        log_once("dsound.fmt",
                 "dsound: wave format tag %u is not PCM; the buffer is treated "
                 "as PCM and will sound wrong if it is compressed",
                 tag);
    }
    b->nchannels = rd16(wfx + WFX_OFF_nChannels);
    b->rate = rd32(wfx + WFX_OFF_nSamplesPerSec);
    b->bits = rd16(wfx + WFX_OFF_wBitsPerSample);
    b->block_align = rd16(wfx + WFX_OFF_nBlockAlign);
    if (!b->nchannels)
        b->nchannels = 1;
    if (!b->rate)
        b->rate = 22050;
    if (!b->bits)
        b->bits = 8;
    if (!b->block_align)
        b->block_align = b->nchannels * (b->bits / 8);
}

void write_wave_format(const ComObj *b, uint32_t wfx) {
    wr16(wfx + WFX_OFF_wFormatTag, WAVE_FORMAT_PCM);
    wr16(wfx + WFX_OFF_nChannels, (uint16_t)b->nchannels);
    wr32(wfx + WFX_OFF_nSamplesPerSec, b->rate);
    wr32(wfx + WFX_OFF_nAvgBytesPerSec, b->rate * b->block_align);
    wr16(wfx + WFX_OFF_nBlockAlign, (uint16_t)b->block_align);
    wr16(wfx + WFX_OFF_wBitsPerSample, (uint16_t)b->bits);
    wr16(wfx + WFX_OFF_cbSize, 0);
}

// ---------------------------------------------------------------------------
// RECOMP_AUDIO_TRACE=N prints the first N audio events - every Lock, Unlock,
// GetCurrentPosition and submission on a playing buffer, with the offsets, the
// lengths and the peak of what was written. A stream that goes quiet is
// always one of a small number of things, and they are told apart by which of
// these lines stops appearing: the guest not asking, the guest asking and
// writing nothing, or the write not reaching the host. Nothing here is on by
// default and nothing here costs anything when it is off.
// ---------------------------------------------------------------------------
// RECOMP_AUDIO_DUMP=<path> writes every run the guest puts into a streaming ring
// to a raw file, in the order it wrote them. Concatenated that way the file is
// the decoded stream itself, which is what makes it comparable against a
// reference decode of the same source: a chunk dropped, repeated or joined at
// the wrong offset shows up as the alignment drifting, and nothing else does.
FILE *audio_dump_file() {
    static FILE *f = nullptr;
    static bool tried = false;
    if (!tried) {
        tried = true;
        if (const char *path = recomp_env("AUDIO_DUMP"))
            f = fopen(path, "wb");
    }
    return f;
}

int audio_trace_budget() {
    static int budget = -1;
    if (budget < 0) {
        const char *v = recomp_env("AUDIO_TRACE");
        budget = v ? (int)strtol(v, nullptr, 0) : 0;
    }
    return budget;
}

bool audio_trace_take() {
    static int left = -1;
    if (left < 0)
        left = audio_trace_budget();
    if (left <= 0)
        return false;
    --left;
    return true;
}

#define ATRACE(...)                                                                                \
    do {                                                                                           \
        if (audio_trace_budget() && audio_trace_take())                                            \
            LOGW(__VA_ARGS__);                                                                     \
    } while (0)

// The loudest sample in a block of guest PCM, as a fraction of full scale.
// It is the one number that separates "the guest handed us silence" from "the
// pipeline swallowed it", and those two have entirely different causes. The
// host measures the same thing on its own side; the point of measuring here as
// well is that the two numbers together say which side lost the sound.
float pcm_peak(uint32_t addr, uint32_t bytes, uint32_t bits) {
    if (!addr || !bytes || !gm_fits(addr, (uint64_t)bytes))
        return 0.0f;
    const uint8_t *p = (const uint8_t *)gm_ptr(addr);
    float peak = 0.0f;
    if (bits == 8) {
        for (uint32_t i = 0; i < bytes; ++i) {
            float v = ((float)p[i] - 128.0f) / 128.0f;
            if (v < 0)
                v = -v;
            if (v > peak)
                peak = v;
        }
    } else {
        for (uint32_t i = 0; i + 1 < bytes; i += 2) {
            int16_t s = (int16_t)(uint16_t)(p[i] | (p[i + 1] << 8));
            float v = (float)s / 32768.0f;
            if (v < 0)
                v = -v;
            if (v > peak)
                peak = v;
        }
    }
    return peak;
}

// Buffers that have already announced themselves. One line per buffer, on its
// first Play, so a live run says what it handed the host without a per-refill
// flood.
std::vector<uint32_t> &announced() {
    static auto *v = new std::vector<uint32_t>();
    return *v;
}

void announce_once(const ComObj *b, uint32_t from) {
    for (uint32_t id : announced())
        if (id == b->id)
            return;
    announced().push_back(b->id);
    LOGV("dsound: buffer %u first play on channel %d: %u Hz, %u ch, %u bit, "
         "%u bytes from %u, %s, peak %.3f",
         b->id, b->channel, b->frequency ? b->frequency : b->rate, b->nchannels, b->bits,
         b->buf_bytes, from, b->looping ? "looping" : "one-shot",
         (double)pcm_peak(b->buf_pixels, b->buf_bytes, b->bits));
}

// RECOMP_AUDIO_DUMP_BUFFERS=<dir> writes the PCM of every distinct sound the
// guest plays as <dir>/buf<id>_<hash>.wav, once per distinct content, at the
// moment of Play. It is the way to listen to what the guest decoded, to tell a
// mis-decoded sample from a mis-scheduled one.
void dump_buffer_wav(const ComObj *b) {
    static const char *dir = recomp_env("AUDIO_DUMP_BUFFERS");
    if (!dir || !*dir || !b->buf_bytes || !gm_fits(b->buf_pixels, (uint64_t)b->buf_bytes))
        return;
    static std::vector<uint64_t> seen;
    const uint8_t *p = (const uint8_t *)gm_ptr(b->buf_pixels);
    uint64_t h = 1469598103934665603ull; // FNV-1a over the data and the format
    for (uint32_t i = 0; i < b->buf_bytes; ++i)
        h = (h ^ p[i]) * 1099511628211ull;
    uint32_t rate = b->frequency ? b->frequency : b->rate;
    h = (h ^ rate) * 1099511628211ull;
    for (uint64_t v : seen)
        if (v == h)
            return;
    seen.push_back(h);
    char path[1024];
    snprintf(path, sizeof path, "%s/buf%u_%016llx.wav", dir, b->id, (unsigned long long)h);
    FILE *f = fopen(path, "wb");
    if (!f) {
        LOGW("dsound: cannot write %s", path);
        return;
    }
    auto w32 = [&](uint32_t v) { fwrite(&v, 4, 1, f); };
    auto w16 = [&](uint16_t v) { fwrite(&v, 2, 1, f); };
    uint32_t ba = b->nchannels * b->bits / 8;
    fwrite("RIFF", 1, 4, f);
    w32(36 + b->buf_bytes);
    fwrite("WAVEfmt ", 1, 8, f);
    w32(16);
    w16(1);
    w16((uint16_t)b->nchannels);
    w32(rate);
    w32(rate * ba);
    w16((uint16_t)ba);
    w16((uint16_t)b->bits);
    fwrite("data", 1, 4, f);
    w32(b->buf_bytes);
    fwrite(p, 1, b->buf_bytes, f);
    fclose(f);
    LOGW("dsound: wrote %s", path);
}

void start_playback_loop(ComObj *b, uint32_t from, bool loop) {
    if (b->is_primary_buffer || b->channel < 0 || !b->buf_pixels)
        return;
    HostAudioPlay p;
    memset(&p, 0, sizeof p);
    p.channel = b->channel;
    p.pcm = gm_ptr(b->buf_pixels);
    p.bytes = b->buf_bytes;
    p.sample_rate = (int32_t)(b->frequency ? b->frequency : b->rate);
    p.channels = (int32_t)b->nchannels;
    p.bits = (int32_t)b->bits;
    p.loop = loop ? 1 : 0;
    calc_3d(b, &p.volume, &p.pan);
    p.start_offset = from;
    announce_once(b, from);
    dump_buffer_wav(b);
    ATRACE("dsound: play buffer %u channel %d from %u, %u bytes, loop %d, peak %.3f", b->id,
           b->channel, from, b->buf_bytes, p.loop,
           (double)pcm_peak(b->buf_pixels, b->buf_bytes, b->bits));
    host_audio_play(&p);
}

void start_playback(ComObj *b, uint32_t from) {
    start_playback_loop(b, from, b->looping);
}

// Hands one run of the ring to the host as a continuation of what is already
// sounding. Returns false when the host refused, which is its way of saying it
// cannot continue a sound and the caller must start one instead.
bool stream_queue_run(ComObj *b, uint32_t off, uint32_t len) {
    if (!len)
        return true;
    // Written where it lies, not scheduled behind what is playing. A ring is
    // memory the cursor runs through, and the offset is known here, so the
    // host can put the bytes exactly where the guest put them instead of
    // treating each run as a buffer that follows the last one. The difference
    // is a lap boundary that costs nothing against one that costs 25 ms of
    // silence, because a full-ring write can only ever arrive just after the
    // previous run has finished playing.
    int32_t taken = host_audio_write(b->channel, gm_ptr(b->buf_pixels + off), off, len);
    // A host with no in-place path says 0, and the run is scheduled behind
    // instead, which is what this did before and still works.
    if (taken <= 0)
        taken = host_audio_queue(b->channel, gm_ptr(b->buf_pixels + off), len);
    // `lead` is how far ahead of the play cursor this run was written. A
    // streamed ring must keep at least a good fraction of a refill ahead, or a
    // frame's stall lets the cursor overtake unplayed bytes and the boundary is
    // a click. `play` and `off` are both ring offsets, so lead is modulo the
    // ring. Gated by RECOMP_AUDIO_TRACE like everything here.
    if (audio_trace_budget()) {
        uint32_t play = live_position(b);
        uint32_t lead = b->buf_bytes ? (off + b->buf_bytes - play) % b->buf_bytes : 0;
        ATRACE("dsound: queue buffer %u channel %d off %u len %u -> %d, play %u, "
               "lead %u, peak %.3f",
               b->id, b->channel, off, len, taken, play, lead,
               (double)pcm_peak(b->buf_pixels + off, len, b->bits));
    }
    return taken > 0;
}

// The guest has just written [off, off+len) of a ring it is playing. Feed it,
// splitting at the wrap because a run that crosses the end of the ring is two
// runs in play order.
bool stream_feed(ComObj *b, StreamRecord *r, uint32_t off, uint32_t len) {
    if (!b->buf_bytes || !len)
        return true;
    // The first feed after a conversion is seeded with the play position, not
    // with the guest's write cursor, so it is not a jump whatever offset it is
    // at. Ring order is checked from the second feed on; the guest's cursor
    // legitimately leads the play cursor by a refill.
    if (r->fed_once && off != r->ring_end) {
        // The traced game writes strictly in ring order. Anything else is a
        // guest doing something this model does not describe, so say so once
        // and follow it rather than silently playing the wrong bytes.
        log_once("dsound.streamjump",
                 "dsound: a streamed buffer was written at %u with %u fed; "
                 "following the guest rather than the ring",
                 off, r->ring_end);
    }
    r->fed_once = true;
    uint32_t first = len;
    uint32_t second = 0;
    if (off + len > b->buf_bytes) {
        first = b->buf_bytes - off;
        second = len - first;
    }
    if (!stream_queue_run(b, off, first))
        return false;
    if (second && !stream_queue_run(b, 0, second))
        return false;
    r->ring_end = (off + len) % b->buf_bytes;
    return true;
}

// ===========================================================================
// IDirectSoundBuffer
// ===========================================================================
void Buffer_GetCaps(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t out = arg(c, 1);
    if (!b || !out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    if (rd32(out) != DSBCAPS_SIZE || !gm_valid(out, DSBCAPS_SIZE)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    wr32(out + 4, b->buf_flags); // dwFlags
    wr32(out + 8, b->buf_bytes); // dwBufferBytes
    wr32(out + 12, 0);           // dwUnlockTransferRate
    wr32(out + 16, 0);           // dwPlayCpuOverhead
    com_ret(c, DS_OK);
}

void Buffer_GetCurrentPosition(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t play = arg(c, 1), write = arg(c, 2);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    service_notifications();
    uint32_t pos = live_position(b);
    b->play_cursor = pos;
    ATRACE("dsound: GetCurrentPosition buffer %u -> play %u (playing %d, "
           "looping %d, channel %d, %u bytes)",
           b->id, pos, (int)b->playing, (int)b->looping, b->channel, b->buf_bytes);
    if (play && gm_valid(play, 4))
        wr32(play, pos);
    // The write cursor leads the play cursor by the mixer's own look-ahead;
    // the region between them is the part DirectSound will not promise. Wine
    // reports ten milliseconds of it, which for the game's 22050 Hz stereo
    // 16-bit music buffer is the 880 bytes its trace shows on every one of
    // three and a half thousand polls.
    if (write && gm_valid(write, 4)) {
        uint32_t align = b->block_align ? b->block_align : 1;
        uint32_t lead = (b->rate ? b->rate : 22050) * align / 100u;
        lead -= lead % align;
        wr32(write, b->buf_bytes ? (pos + lead) % b->buf_bytes : 0u);
    }
    com_ret(c, DS_OK);
}

void Buffer_GetFormat(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t out = arg(c, 1), cap = arg(c, 2), written = arg(c, 3);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    if (!out) {
        if (written && gm_valid(written, 4))
            wr32(written, WFX_SIZE);
        com_ret(c, DS_OK);
        return;
    }
    if (cap < WFX_SIZE || !gm_fits(out, WFX_SIZE)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    write_wave_format(b, out);
    if (written && gm_valid(written, 4))
        wr32(written, WFX_SIZE);
    com_ret(c, DS_OK);
}

void Buffer_GetVolume(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t out = arg(c, 1);
    if (!b || !out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    wr32(out, (uint32_t)b->volume);
    com_ret(c, DS_OK);
}

void Buffer_GetPan(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t out = arg(c, 1);
    if (!b || !out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    wr32(out, (uint32_t)b->pan);
    com_ret(c, DS_OK);
}

void Buffer_GetFrequency(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t out = arg(c, 1);
    if (!b || !out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    wr32(out, b->frequency ? b->frequency : b->rate);
    com_ret(c, DS_OK);
}

void Buffer_GetStatus(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t out = arg(c, 1);
    if (!b || !out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    // A buffer the host has finished playing is no longer playing, so ask.
    service_notifications();
    if (b->playing && !b->looping && b->channel >= 0 && !host_audio_is_playing(b->channel))
        b->playing = false;
    uint32_t st = 0;
    if (b->playing)
        st |= DSBSTATUS_PLAYING;
    if (b->playing && b->looping)
        st |= DSBSTATUS_LOOPING;
    wr32(out, st);
    com_ret(c, DS_OK);
}

DX_STUB(Buffer_Initialize, DSERR_INVALIDCALL) // already initialised

// Expose one or two guest memory spans for a circular sound-buffer lock.
// Validate offsets and lengths before returning pointers into the guest allocation.
void Buffer_Lock(X86 *c) {
    ComObj *b = this_buffer(c);
    service_notifications();
    uint32_t off = arg(c, 1), bytes = arg(c, 2);
    uint32_t p1 = arg(c, 3), n1 = arg(c, 4);
    uint32_t p2 = arg(c, 5), n2 = arg(c, 6);
    uint32_t flags = arg(c, 7);
    if (!b || !p1 || !n1) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    // Both required out-parameters, and the optional pair when supplied, must
    // be writable before anything is stored through them.
    if (!gm_valid(p1, 4) || !gm_valid(n1, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    if ((p2 && !gm_valid(p2, 4)) || (n2 && !gm_valid(n2, 4))) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    if (!b->buf_pixels || !b->buf_bytes) {
        com_ret(c, DSERR_INVALIDCALL);
        return;
    }

    // DSBLOCK_ENTIREBUFFER = 2 ignores dwBytes.
    if (flags & 2u) {
        off = 0;
        bytes = b->buf_bytes;
    }
    // DSBLOCK_FROMWRITECURSOR = 1 starts at the write cursor.
    if (flags & 1u)
        off = b->write_cursor;
    if (off >= b->buf_bytes) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    if (bytes > b->buf_bytes)
        bytes = b->buf_bytes;

    // A lock that runs off the end wraps into a second region, which is the
    // whole reason Lock has two output pointers.
    uint32_t first = bytes;
    uint32_t second = 0;
    if (off + bytes > b->buf_bytes) {
        first = b->buf_bytes - off;
        second = bytes - first;
    }
    // The regions handed out are inside the buffer by construction, but the
    // buffer's own span is re-checked so a corrupted record cannot hand the
    // guest a pointer past the arena.
    if (!gm_fits(b->buf_pixels, (uint64_t)b->buf_bytes)) {
        com_ret(c, DSERR_INVALIDCALL);
        return;
    }
    wr32(p1, b->buf_pixels + off);
    wr32(n1, first);
    if (p2)
        wr32(p2, second ? b->buf_pixels : 0);
    if (n2)
        wr32(n2, second);
    b->lock_off = off;
    b->lock_len = bytes;
    ATRACE("dsound: Lock buffer %u off %u bytes %u flags %x -> %u + %u "
           "(playing %d, looping %d, channel %d)",
           b->id, off, bytes, flags, first, second, (int)b->playing, (int)b->looping, b->channel);
    com_ret(c, DS_OK);
}

void Buffer_Play(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t flags = arg(c, 3);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    b->looping = (flags & DSBPLAY_LOOPING) != 0;
    if (b->is_primary_buffer) {
        // Playing the primary buffer only keeps the mixer running; there is no
        // sample to hear. Recording the state is the whole contract.
        b->playing = true;
        com_ret(c, DS_OK);
        return;
    }
    if (b->channel < 0)
        b->channel = dx_alloc_audio_channel();
    if (b->channel < 0) {
        com_ret(c, DSERR_ALLOCATED);
        return;
    }
    b->playing = true;
    // A fresh Play is a fresh sound: whatever the last one had been converted
    // into is over, and the ring is a loop again until the guest writes.
    stream_reset(b->id);
    start_playback(b, b->play_cursor);
    if (NotifyRecord *n = notify_for(b->id)) {
        n->last = b->play_cursor;
        n->tracking = true;
    }
    LOGV("dsound: play channel %d, %u bytes, %u Hz, %u ch, %u bit%s", b->channel, b->buf_bytes,
         b->frequency ? b->frequency : b->rate, b->nchannels, b->bits,
         b->looping ? ", looping" : "");
    com_ret(c, DS_OK);
}

void Buffer_SetCurrentPosition(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t pos = arg(c, 1);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    if (b->buf_bytes && pos >= b->buf_bytes) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    b->play_cursor = pos;
    // Moving the cursor is a discontinuity by definition, so a stream that had
    // been converted starts again from here.
    stream_reset(b->id);
    if (b->playing)
        start_playback(b, pos);
    com_ret(c, DS_OK);
}

void Buffer_SetFormat(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t wfx = arg(c, 1);
    if (!b || !wfx) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    // Only the primary buffer's format may be set, exactly as DirectSound
    // documents; a secondary buffer's format is fixed at creation.
    if (!b->is_primary_buffer) {
        com_ret(c, DSERR_INVALIDCALL);
        return;
    }
    read_wave_format(b, wfx);
    LOGV("dsound: primary format %u Hz, %u ch, %u bit", b->rate, b->nchannels, b->bits);
    com_ret(c, DS_OK);
}

void Buffer_SetVolume(X86 *c) {
    ComObj *b = this_buffer(c);
    int32_t v = (int32_t)arg(c, 1);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    if (v > DSBVOLUME_MAX || v < DSBVOLUME_MIN) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    b->volume = v;
    // A 3D buffer folds its own volume into the distance calculation; a plain
    // one applies it directly. update_channel_mix does whichever applies.
    update_channel_mix(b);
    com_ret(c, DS_OK);
}

void Buffer_SetPan(X86 *c) {
    ComObj *b = this_buffer(c);
    int32_t v = (int32_t)arg(c, 1);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    if (v > DSBPAN_RIGHT || v < DSBPAN_LEFT) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    b->pan = v;
    // For a 3D buffer in NORMAL/HEADRELATIVE mode the pan comes from the 3D
    // position and this is stored but does not reach the mix, which matches
    // DirectSound's documented "SetPan has no effect on a 3D buffer". In
    // DS3DMODE_DISABLE it does apply, as Wine's disabled path does.
    update_channel_mix(b);
    com_ret(c, DS_OK);
}

void Buffer_SetFrequency(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t hz = arg(c, 1);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    // DSBFREQUENCY_ORIGINAL is 0 and restores the format's own rate.
    if (hz && (hz < 100 || hz > 100000)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    b->frequency = hz;
    if (b->channel >= 0)
        host_audio_set_frequency(b->channel, hz ? hz : b->rate);
    com_ret(c, DS_OK);
}

void Buffer_Stop(X86 *c) {
    ComObj *b = this_buffer(c);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    // Stop keeps the position it stopped at, so a later Play resumes there.
    if (b->playing && b->channel >= 0) {
        b->play_cursor = live_position(b);
        host_audio_stop(b->channel);
    }
    b->playing = false;
    stream_reset(b->id);
    signal_stop_positions(b);
    com_ret(c, DS_OK);
}

// Commit a sound-buffer write and forward the affected PCM spans to native playback.
// Handle circular writes and streaming progress without restarting unchanged audio.
void Buffer_Unlock(X86 *c) {
    ComObj *b = this_buffer(c);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    // The guest has just rewritten part of the buffer, and a buffer that is
    // already playing has to hear it. How depends on what the guest is doing:
    // writing into a ring it is looping is a stream and is fed by appending,
    // anything else is re-submitted.
    uint32_t off = b->lock_off, len = b->lock_len;
    b->lock_off = b->lock_len = 0;
    // The step across the seam between this run and the one before it, in the
    // ring rather than in the call sequence: the sample immediately before
    // `off` is the last one the previous run wrote, because the guest writes
    // in ring order. Measured here so a discontinuity can be placed on the
    // guest's side of host_audio_write rather than argued about across it.
    if (audio_trace_budget() && len && b->buf_bytes && b->bits == 16 &&
        gm_fits(b->buf_pixels, (uint64_t)b->buf_bytes)) {
        uint32_t align = b->block_align ? b->block_align : 4;
        uint32_t prev = (off + b->buf_bytes - align) % b->buf_bytes;
        const uint8_t *ring = (const uint8_t *)gm_ptr(b->buf_pixels);
        float worst = 0.0f;
        for (uint32_t ch = 0; ch < b->nchannels && ch * 2 + 1 < align; ++ch) {
            const uint8_t *a = ring + prev + ch * 2;
            const uint8_t *d = ring + off + ch * 2;
            int16_t last = (int16_t)(uint16_t)(a[0] | (a[1] << 8));
            int16_t first = (int16_t)(uint16_t)(d[0] | (d[1] << 8));
            float step = (float)(first - last) / 32768.0f;
            if (step < 0)
                step = -step;
            if (step > worst)
                worst = step;
        }
        // The two samples either side, not only the distance between them.
        // A decoder that restarts its predictor at every chunk begins each one
        // near zero whatever the signal was doing, so "large, then small" is a
        // different fault from "large, then large but wrong".
        const uint8_t *a0 = ring + prev;
        const uint8_t *d0 = ring + off;
        int16_t last0 = (int16_t)(uint16_t)(a0[0] | (a0[1] << 8));
        int16_t first0 = (int16_t)(uint16_t)(d0[0] | (d0[1] << 8));
        // And the same measurement inside the run, which is the control. If
        // the boundaries are no worse than the interior then the audio simply
        // has transients and the seams are innocent; if they are far worse,
        // the boundary is where something is going wrong.
        uint32_t big = 0, pairs = 0, worst_interior = 0;
        for (uint32_t k = align; k < len; k += align) {
            uint32_t at = (off + k) % b->buf_bytes;
            uint32_t be = (off + k - align) % b->buf_bytes;
            const uint8_t *q0 = ring + be;
            const uint8_t *q1 = ring + at;
            int16_t v0 = (int16_t)(uint16_t)(q0[0] | (q0[1] << 8));
            int16_t v1 = (int16_t)(uint16_t)(q1[0] | (q1[1] << 8));
            int step = v1 - v0;
            if (step < 0)
                step = -step;
            if (step > 8192)
                ++big; // a quarter of full scale
            if (step > (int)worst_interior)
                worst_interior = (uint32_t)step;
            ++pairs;
        }
        // The interior's worst step, not only its rate. A boundary step is
        // only remarkable next to the largest step the signal makes anyway.
        ATRACE("dsound: seam at %u: step %.3f, last %d first %d, "
               "interior %u big of %u, interior worst %.3f",
               off, (double)worst, (int)last0, (int)first0, big, pairs,
               (double)worst_interior / 32768.0);
    }

    // Both halves of a run that wraps. Measuring only the first and reporting
    // zero for the rest made every wrapping run look like silence, which is a
    // trace that invents the fault it is there to find.
    if (audio_trace_budget()) {
        uint32_t first = len, second = 0;
        if (b->buf_bytes && off + len > b->buf_bytes) {
            first = b->buf_bytes - off;
            second = len - first;
        }
        float peak = pcm_peak(b->buf_pixels + off, first, b->bits);
        if (second) {
            float p2 = pcm_peak(b->buf_pixels, second, b->bits);
            if (p2 > peak)
                peak = p2;
        }
        ATRACE("dsound: Unlock buffer %u off %u len %u%s (playing %d, looping %d, "
               "channel %d, peak %.3f)",
               b->id, off, len, second ? " wrapping" : "", (int)b->playing, (int)b->looping,
               b->channel, (double)peak);
    }
    if (!b->playing || b->channel < 0 || !b->buf_bytes) {
        com_ret(c, DS_OK);
        return;
    }

    if (FILE *dump = audio_dump_file()) {
        if (len && b->buf_bytes && gm_fits(b->buf_pixels, (uint64_t)b->buf_bytes)) {
            const uint8_t *ring = (const uint8_t *)gm_ptr(b->buf_pixels);
            uint32_t first = len, second = 0;
            if (off + len > b->buf_bytes) {
                first = b->buf_bytes - off;
                second = len - first;
            }
            fwrite(ring + off, 1, first, dump);
            if (second)
                fwrite(ring, 1, second, dump);
        }
    }

    StreamRecord *r = stream_for(b->id, true);
    if (b->looping && len && !r->refused) {
        // The guest is writing into a ring it is playing, which is a stream
        // however DirectSound spells it. Ring order from the play cursor is
        // play order, so the rest of the current lap becomes a one-shot and
        // everything written afterwards is appended behind it. Nothing
        // restarts, which is the whole point.
        // The guest is writing into a ring it is playing, which is a stream
        // however DirectSound spells it. host_audio_stream converts it at the
        // cursor: one join, nothing repeated and nothing skipped, though the
        // host does stop and reschedule the voice to do it, so there is a
        // seam of a millisecond or two. It happens once per sound and on a
        // ring that holds silence at that moment. Every region unlocked
        // afterwards is appended behind it, and none of those restarts
        // anything, which is the part that was audible.
        bool was_active = r->active;
        if (!r->active) {
            int32_t at = host_audio_stream(b->channel);
            if (at < 0) {
                r->refused = true;
                log_once("dsound.nostream",
                         "dsound: this host cannot continue a sound, so a buffer "
                         "the guest streams into is re-submitted at every refill; "
                         "the join is audible");
            } else {
                r->active = true;
                r->ring_end = (uint32_t)at % (b->buf_bytes ? b->buf_bytes : 1u);
                ATRACE("dsound: buffer %u is a stream now, from %d", b->id, at);
            }
        }
        if (r->active) {
            (void)was_active;
            if (stream_feed(b, r, off, len)) {
                b->play_cursor = live_position(b);
                if (NotifyRecord *n = notify_for(b->id)) {
                    n->last = b->play_cursor;
                    n->tracking = true;
                }
                com_ret(c, DS_OK);
                return;
            }
            // Refused with a stream established. The host accepts a late chunk
            // after an underrun, so this is a voice that has gone rather than
            // one that is behind: convert again from wherever it has reached.
            int32_t again = host_audio_stream(b->channel);
            if (again >= 0) {
                r->ring_end = (uint32_t)again % (b->buf_bytes ? b->buf_bytes : 1u);
                log_once("dsound.restream",
                         "dsound: a streamed buffer's voice was replaced between "
                         "refills and has been converted again");
                if (stream_feed(b, r, off, len)) {
                    b->play_cursor = live_position(b);
                    com_ret(c, DS_OK);
                    return;
                }
            }
            r->active = false;
            r->refused = true;
            r->ring_end = 0;
            log_once("dsound.nostream", "dsound: this host cannot continue a sound, so a buffer "
                                        "the guest streams into is re-submitted at every refill; "
                                        "the join is audible");
        }
        uint32_t pos = live_position(b);
        start_playback_loop(b, pos, true);
        b->play_cursor = pos;
        com_ret(c, DS_OK);
        return;
    }

    // Not a stream: a buffer that is playing has to hear what was just written
    // into it, and the only way to say so is to submit it again. From the live
    // cursor, not from the last one anybody asked about - the writer never
    // calls GetCurrentPosition, so a stale cursor restarts the sound at the
    // top of the buffer every time.
    b->play_cursor = live_position(b);
    start_playback(b, b->play_cursor);
    if (NotifyRecord *n = notify_for(b->id)) {
        n->last = b->play_cursor;
        n->tracking = true;
    }
    com_ret(c, DS_OK);
}

void Buffer_Restore(X86 *c) {
    com_ret(c, DS_OK);
} // buffers are never lost

const ComMethod g_dsbuffer[] = {
    {"QueryInterface", 3, com_QueryInterface},
    {"AddRef", 1, com_AddRef},
    {"Release", 1, com_Release},
    {"GetCaps", 2, Buffer_GetCaps},
    {"GetCurrentPosition", 3, Buffer_GetCurrentPosition},
    {"GetFormat", 4, Buffer_GetFormat},
    {"GetVolume", 2, Buffer_GetVolume},
    {"GetPan", 2, Buffer_GetPan},
    {"GetFrequency", 2, Buffer_GetFrequency},
    {"GetStatus", 2, Buffer_GetStatus},
    {"Initialize", 3, Buffer_Initialize},
    {"Lock", 8, Buffer_Lock},
    {"Play", 4, Buffer_Play},
    {"SetCurrentPosition", 2, Buffer_SetCurrentPosition},
    {"SetFormat", 2, Buffer_SetFormat},
    {"SetVolume", 2, Buffer_SetVolume},
    {"SetPan", 2, Buffer_SetPan},
    {"SetFrequency", 2, Buffer_SetFrequency},
    {"Stop", 1, Buffer_Stop},
    {"Unlock", 5, Buffer_Unlock},
    {"Restore", 1, Buffer_Restore},
};

// ===========================================================================
// IDirectSoundBuffer8.
//
// IDirectSoundBuffer8 is IDirectSoundBuffer plus SetFX, AcquireResources and
// GetObjectInPath. The game asks for it in its streamed-voice path
// (Ghidra 0x005684e2: CreateSoundBuffer then QueryInterface(IID_IDirectSoundBuffer8));
// a refusal there is the E_NOINTERFACE the RSMusic path logs as
// "RSMusic: Error downloading effect", and it stops the music buffer from ever
// being played. All three added methods are unused by the reference: the only
// indirect calls at buffer vtable offsets 0x54/0x58/0x5c in the image are on
// unrelated objects (0x0045cdd0, 0x00467410, 0x004e65b0). They are declared
// here so the interface exists and fail by name if that ever changes.
//
// The three slots must return a real failure rather than a plausible success,
// because silently accepting SetFX would claim an effect that was never
// applied.
// ===========================================================================
DX_STUB(Buffer8_SetFX, E_NOTIMPL)
DX_STUB(Buffer8_AcquireResources, E_NOTIMPL)
DX_STUB(Buffer8_GetObjectInPath, E_NOTIMPL)

const ComMethod g_dsbuffer8[] = {
    {"QueryInterface", 3, com_QueryInterface},
    {"AddRef", 1, com_AddRef},
    {"Release", 1, com_Release},
    {"GetCaps", 2, Buffer_GetCaps},
    {"GetCurrentPosition", 3, Buffer_GetCurrentPosition},
    {"GetFormat", 4, Buffer_GetFormat},
    {"GetVolume", 2, Buffer_GetVolume},
    {"GetPan", 2, Buffer_GetPan},
    {"GetFrequency", 2, Buffer_GetFrequency},
    {"GetStatus", 2, Buffer_GetStatus},
    {"Initialize", 3, Buffer_Initialize},
    {"Lock", 8, Buffer_Lock},
    {"Play", 4, Buffer_Play},
    {"SetCurrentPosition", 2, Buffer_SetCurrentPosition},
    {"SetFormat", 2, Buffer_SetFormat},
    {"SetVolume", 2, Buffer_SetVolume},
    {"SetPan", 2, Buffer_SetPan},
    {"SetFrequency", 2, Buffer_SetFrequency},
    {"Stop", 1, Buffer_Stop},
    {"Unlock", 5, Buffer_Unlock},
    {"Restore", 1, Buffer_Restore},
    {"SetFX", 3, Buffer8_SetFX},
    {"AcquireResources", 4, Buffer8_AcquireResources},
    {"GetObjectInPath", 7, Buffer8_GetObjectInPath},
};

// ===========================================================================
// IDirectSound3DBuffer - a view on the same buffer object.
// ===========================================================================
// DS3DBUFFER is 64 bytes (Wine include/dsound.h): dwSize, vPosition,
// vVelocity, dwInsideConeAngle, dwOutsideConeAngle, vConeOrientation,
// lConeOutsideVolume, flMinDistance, flMaxDistance, dwMode. The previous code
// treated it as 76 bytes and wrote MinDistance into the cone-outside-volume
// slot; that is corrected here.
void B3D_GetAllParameters(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t out = arg(c, 1);
    if (!b || !out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    uint32_t size = rd32(out);
    if (size < 64 || !gm_valid(out, size)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    gm_zero(out + 4, size - 4);
    for (int i = 0; i < 3; ++i)
        wrf32(out + 4 + 4u * (uint32_t)i, b->pos3d[i]);
    for (int i = 0; i < 3; ++i)
        wrf32(out + 16 + 4u * (uint32_t)i, b->vel3d[i]);
    wr32(out + 28, b->cone_inside);
    wr32(out + 32, b->cone_outside);
    for (int i = 0; i < 3; ++i)
        wrf32(out + 36 + 4u * (uint32_t)i, b->cone_orient[i]);
    wr32(out + 48, (uint32_t)b->cone_outside_volume);
    wrf32(out + 52, b->min3d);
    wrf32(out + 56, b->max3d);
    wr32(out + 60, b->mode3d);
    com_ret(c, DS_OK);
}

void B3D_GetPosition(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t out = arg(c, 1);
    if (!b || !out || !gm_valid(out, 12)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    for (int i = 0; i < 3; ++i)
        wrf32(out + 4u * (uint32_t)i, b->pos3d[i]);
    com_ret(c, DS_OK);
}

void B3D_GetVelocity(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t out = arg(c, 1);
    if (!b || !out || !gm_valid(out, 12)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    for (int i = 0; i < 3; ++i)
        wrf32(out + 4u * (uint32_t)i, b->vel3d[i]);
    com_ret(c, DS_OK);
}

void B3D_GetConeAngles(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t inside = arg(c, 1), outside = arg(c, 2);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    if (inside && gm_valid(inside, 4))
        wr32(inside, b->cone_inside);
    if (outside && gm_valid(outside, 4))
        wr32(outside, b->cone_outside);
    com_ret(c, DS_OK);
}

void B3D_GetConeOrientation(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t out = arg(c, 1);
    if (!b || !out || !gm_valid(out, 12)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    for (int i = 0; i < 3; ++i)
        wrf32(out + 4u * (uint32_t)i, b->cone_orient[i]);
    com_ret(c, DS_OK);
}

void B3D_GetConeOutsideVolume(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t out = arg(c, 1);
    if (!b || !out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    wr32(out, (uint32_t)b->cone_outside_volume);
    com_ret(c, DS_OK);
}

void B3D_GetMaxDistance(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t out = arg(c, 1);
    if (!b || !out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    wrf32(out, b->max3d);
    com_ret(c, DS_OK);
}

void B3D_GetMinDistance(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t out = arg(c, 1);
    if (!b || !out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    wrf32(out, b->min3d);
    com_ret(c, DS_OK);
}

void B3D_GetMode(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t out = arg(c, 1);
    if (!b || !out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    wr32(out, b->mode3d);
    com_ret(c, DS_OK);
}

void B3D_SetPosition(X86 *c) {
    ComObj *b = this_buffer(c);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    // The three coordinates are floats passed by value on the stack.
    uint32_t x = arg(c, 1), y = arg(c, 2), z = arg(c, 3);
    memcpy(&b->pos3d[0], &x, 4);
    memcpy(&b->pos3d[1], &y, 4);
    memcpy(&b->pos3d[2], &z, 4);
    if (arg(c, 4) == DS3D_IMMEDIATE)
        update_channel_mix(b);
    com_ret(c, DS_OK);
}

void B3D_SetVelocity(X86 *c) {
    ComObj *b = this_buffer(c);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    uint32_t x = arg(c, 1), y = arg(c, 2), z = arg(c, 3);
    memcpy(&b->vel3d[0], &x, 4);
    memcpy(&b->vel3d[1], &y, 4);
    memcpy(&b->vel3d[2], &z, 4);
    if (b->vel3d[0] != 0.0f || b->vel3d[1] != 0.0f || b->vel3d[2] != 0.0f)
        log_once("dsound.3ddoppler",
                 "dsound: 3D velocity is set but Doppler is not applied to the mix");
    if (arg(c, 4) == DS3D_IMMEDIATE)
        update_channel_mix(b);
    com_ret(c, DS_OK);
}

void B3D_SetConeAngles(X86 *c) {
    ComObj *b = this_buffer(c);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    b->cone_inside = arg(c, 1);
    b->cone_outside = arg(c, 2);
    log_once("dsound.3dcone",
             "dsound: 3D cone angles (%u/%u) are stored but not applied to the mix", b->cone_inside,
             b->cone_outside);
    if (arg(c, 3) == DS3D_IMMEDIATE)
        update_channel_mix(b);
    com_ret(c, DS_OK);
}

void B3D_SetConeOrientation(X86 *c) {
    ComObj *b = this_buffer(c);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    uint32_t x = arg(c, 1), y = arg(c, 2), z = arg(c, 3);
    memcpy(&b->cone_orient[0], &x, 4);
    memcpy(&b->cone_orient[1], &y, 4);
    memcpy(&b->cone_orient[2], &z, 4);
    log_once("dsound.3dcone",
             "dsound: a 3D cone orientation is set but cones are not applied to the mix");
    if (arg(c, 4) == DS3D_IMMEDIATE)
        update_channel_mix(b);
    com_ret(c, DS_OK);
}

void B3D_SetConeOutsideVolume(X86 *c) {
    ComObj *b = this_buffer(c);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    b->cone_outside_volume = (int32_t)arg(c, 1);
    log_once("dsound.3dcone",
             "dsound: a 3D cone outside volume is set but cones are not applied to the mix");
    if (arg(c, 2) == DS3D_IMMEDIATE)
        update_channel_mix(b);
    com_ret(c, DS_OK);
}

void B3D_SetMaxDistance(X86 *c) {
    ComObj *b = this_buffer(c);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    uint32_t v = arg(c, 1);
    memcpy(&b->max3d, &v, 4);
    if (arg(c, 2) == DS3D_IMMEDIATE)
        update_channel_mix(b);
    com_ret(c, DS_OK);
}

void B3D_SetMinDistance(X86 *c) {
    ComObj *b = this_buffer(c);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    uint32_t v = arg(c, 1);
    memcpy(&b->min3d, &v, 4);
    if (arg(c, 2) == DS3D_IMMEDIATE)
        update_channel_mix(b);
    com_ret(c, DS_OK);
}

void B3D_SetMode(X86 *c) {
    ComObj *b = this_buffer(c);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    b->mode3d = arg(c, 1);
    if (b->mode3d != DS3DMODE_NORMAL && b->mode3d != DS3DMODE_HEADRELATIVE &&
        b->mode3d != DS3DMODE_DISABLE)
        log_once("dsound.3dmode", "dsound: unknown 3D mode %u; treating it as normal", b->mode3d);
    if (arg(c, 2) == DS3D_IMMEDIATE)
        update_channel_mix(b);
    com_ret(c, DS_OK);
}

void B3D_SetAllParameters(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t in = arg(c, 1);
    if (!b || !in || !gm_valid(in, 64)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    for (int i = 0; i < 3; ++i)
        b->pos3d[i] = rdf32(in + 4 + 4u * (uint32_t)i);
    for (int i = 0; i < 3; ++i)
        b->vel3d[i] = rdf32(in + 16 + 4u * (uint32_t)i);
    b->cone_inside = rd32(in + 28);
    b->cone_outside = rd32(in + 32);
    for (int i = 0; i < 3; ++i)
        b->cone_orient[i] = rdf32(in + 36 + 4u * (uint32_t)i);
    b->cone_outside_volume = (int32_t)rd32(in + 48);
    b->min3d = rdf32(in + 52);
    b->max3d = rdf32(in + 56);
    b->mode3d = rd32(in + 60);
    if (b->vel3d[0] != 0.0f || b->vel3d[1] != 0.0f || b->vel3d[2] != 0.0f)
        log_once("dsound.3ddoppler",
                 "dsound: 3D velocity is set but Doppler is not applied to the mix");
    if (arg(c, 2) == DS3D_IMMEDIATE)
        update_channel_mix(b);
    com_ret(c, DS_OK);
}

const ComMethod g_ds3dbuffer[] = {
    {"QueryInterface", 3, com_QueryInterface},
    {"AddRef", 1, com_AddRef},
    {"Release", 1, com_Release},
    {"GetAllParameters", 2, B3D_GetAllParameters},
    {"GetConeAngles", 3, B3D_GetConeAngles},
    {"GetConeOrientation", 2, B3D_GetConeOrientation},
    {"GetConeOutsideVolume", 2, B3D_GetConeOutsideVolume},
    {"GetMaxDistance", 2, B3D_GetMaxDistance},
    {"GetMinDistance", 2, B3D_GetMinDistance},
    {"GetMode", 2, B3D_GetMode},
    {"GetPosition", 2, B3D_GetPosition},
    {"GetVelocity", 2, B3D_GetVelocity},
    {"SetAllParameters", 3, B3D_SetAllParameters},
    {"SetConeAngles", 4, B3D_SetConeAngles},
    {"SetConeOrientation", 5, B3D_SetConeOrientation},
    {"SetConeOutsideVolume", 3, B3D_SetConeOutsideVolume},
    {"SetMaxDistance", 3, B3D_SetMaxDistance},
    {"SetMinDistance", 3, B3D_SetMinDistance},
    {"SetMode", 3, B3D_SetMode},
    {"SetPosition", 5, B3D_SetPosition},
    {"SetVelocity", 5, B3D_SetVelocity},
};

// ===========================================================================
// IDirectSound3DListener - a view on the DirectSound object.
// ===========================================================================
void L3D_GetPosition(X86 *c) {
    uint32_t out = arg(c, 1);
    if (!out || !gm_valid(out, 12)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    for (int i = 0; i < 3; ++i)
        wrf32(out + 4u * (uint32_t)i, g_listener[i]);
    com_ret(c, DS_OK);
}

void L3D_SetPosition(X86 *c) {
    uint32_t x = arg(c, 1), y = arg(c, 2), z = arg(c, 3);
    memcpy(&g_listener[0], &x, 4);
    memcpy(&g_listener[1], &y, 4);
    memcpy(&g_listener[2], &z, 4);
    if (arg(c, 4) == DS3D_IMMEDIATE)
        update_all_3d();
    com_ret(c, DS_OK);
}

void L3D_GetOrientation(X86 *c) {
    uint32_t front = arg(c, 1), top = arg(c, 2);
    if (front && gm_valid(front, 12))
        for (int i = 0; i < 3; ++i)
            wrf32(front + 4u * (uint32_t)i, g_listener[3 + i]);
    if (top && gm_valid(top, 12))
        for (int i = 0; i < 3; ++i)
            wrf32(top + 4u * (uint32_t)i, g_listener[6 + i]);
    com_ret(c, DS_OK);
}

void L3D_SetOrientation(X86 *c) {
    // Read the six floats before touching the stored listener, because Wine
    // refuses an orientation whose front and top are parallel and such a call
    // must leave the previous pair in place.
    V3 front, top;
    float v[6];
    for (int i = 0; i < 6; ++i) {
        uint32_t raw = arg(c, 1 + i);
        memcpy(&v[i], &raw, 4);
    }
    front = {v[0], v[1], v[2]};
    top = {v[3], v[4], v[5]};
    if (v3_angle(front, top) == 0.0f) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    for (int i = 0; i < 6; ++i)
        g_listener[3 + i] = v[i];
    if (arg(c, 7) == DS3D_IMMEDIATE)
        update_all_3d();
    com_ret(c, DS_OK);
}

void L3D_GetVelocity(X86 *c) {
    uint32_t out = arg(c, 1);
    if (!out || !gm_valid(out, 12)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    for (int i = 0; i < 3; ++i)
        wrf32(out + 4u * (uint32_t)i, g_listener[9 + i]);
    com_ret(c, DS_OK);
}

void L3D_SetVelocity(X86 *c) {
    uint32_t x = arg(c, 1), y = arg(c, 2), z = arg(c, 3);
    memcpy(&g_listener[9], &x, 4);
    memcpy(&g_listener[10], &y, 4);
    memcpy(&g_listener[11], &z, 4);
    if (g_listener[9] != 0.0f || g_listener[10] != 0.0f || g_listener[11] != 0.0f)
        log_once("dsound.3ddoppler",
                 "dsound: listener velocity is set but Doppler is not applied to the mix");
    if (arg(c, 4) == DS3D_IMMEDIATE)
        update_all_3d();
    com_ret(c, DS_OK);
}

void L3D_GetAllParameters(X86 *c) {
    uint32_t out = arg(c, 1);
    // DS3DLISTENER: dwSize, vPosition, vVelocity, vOrientFront, vOrientTop,
    // flDistanceFactor, flRolloffFactor, flDopplerFactor = 64 bytes (4 + four
    // 12-byte vectors + three floats). Wine checks the same sizeof().
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    uint32_t size = rd32(out);
    if (size < 64 || !gm_valid(out, size)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    gm_zero(out + 4, size - 4);
    for (int i = 0; i < 3; ++i)
        wrf32(out + 4 + 4u * (uint32_t)i, g_listener[i]);
    for (int i = 0; i < 3; ++i)
        wrf32(out + 16 + 4u * (uint32_t)i, g_listener[9 + i]);
    for (int i = 0; i < 3; ++i)
        wrf32(out + 28 + 4u * (uint32_t)i, g_listener[3 + i]);
    for (int i = 0; i < 3; ++i)
        wrf32(out + 40 + 4u * (uint32_t)i, g_listener[6 + i]);
    wrf32(out + 52, g_distance_factor);
    wrf32(out + 56, g_rolloff_factor);
    wrf32(out + 60, g_doppler_factor);
    com_ret(c, DS_OK);
}

void L3D_SetAllParameters(X86 *c) {
    uint32_t in = arg(c, 1);
    if (!in || !gm_valid(in, 64)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    for (int i = 0; i < 3; ++i)
        g_listener[i] = rdf32(in + 4 + 4u * (uint32_t)i);
    for (int i = 0; i < 3; ++i)
        g_listener[9 + i] = rdf32(in + 16 + 4u * (uint32_t)i);
    for (int i = 0; i < 3; ++i)
        g_listener[3 + i] = rdf32(in + 28 + 4u * (uint32_t)i);
    for (int i = 0; i < 3; ++i)
        g_listener[6 + i] = rdf32(in + 40 + 4u * (uint32_t)i);
    g_distance_factor = rdf32(in + 52);
    g_rolloff_factor = rdf32(in + 56);
    g_doppler_factor = rdf32(in + 60);
    if (g_listener[9] != 0.0f || g_listener[10] != 0.0f || g_listener[11] != 0.0f ||
        g_doppler_factor != 1.0f)
        log_once(
            "dsound.3ddoppler",
            "dsound: a listener Doppler parameter is set but Doppler is not applied to the mix");
    if (arg(c, 2) == DS3D_IMMEDIATE)
        update_all_3d();
    com_ret(c, DS_OK);
}

void L3D_GetDistanceFactor(X86 *c) {
    uint32_t out = arg(c, 1);
    if (out && gm_valid(out, 4))
        wrf32(out, g_distance_factor);
    com_ret(c, DS_OK);
}
void L3D_GetDopplerFactor(X86 *c) {
    uint32_t out = arg(c, 1);
    if (out && gm_valid(out, 4))
        wrf32(out, g_doppler_factor);
    com_ret(c, DS_OK);
}
void L3D_GetRolloffFactor(X86 *c) {
    uint32_t out = arg(c, 1);
    if (out && gm_valid(out, 4))
        wrf32(out, g_rolloff_factor);
    com_ret(c, DS_OK);
}
void L3D_SetDistanceFactor(X86 *c) {
    uint32_t v = arg(c, 1);
    memcpy(&g_distance_factor, &v, 4);
    if (arg(c, 2) == DS3D_IMMEDIATE)
        update_all_3d();
    com_ret(c, DS_OK);
}
void L3D_SetDopplerFactor(X86 *c) {
    uint32_t v = arg(c, 1);
    memcpy(&g_doppler_factor, &v, 4);
    if (g_doppler_factor != 1.0f)
        log_once("dsound.3ddoppler",
                 "dsound: the Doppler factor is changed but Doppler is not applied to the mix");
    if (arg(c, 2) == DS3D_IMMEDIATE)
        update_all_3d();
    com_ret(c, DS_OK);
}
void L3D_SetRolloffFactor(X86 *c) {
    uint32_t v = arg(c, 1);
    memcpy(&g_rolloff_factor, &v, 4);
    if (arg(c, 2) == DS3D_IMMEDIATE)
        update_all_3d();
    com_ret(c, DS_OK);
}
void L3D_CommitDeferredSettings(X86 *c) {
    update_all_3d();
    com_ret(c, DS_OK);
}

const ComMethod g_ds3dlistener[] = {
    {"QueryInterface", 3, com_QueryInterface},
    {"AddRef", 1, com_AddRef},
    {"Release", 1, com_Release},
    {"GetAllParameters", 2, L3D_GetAllParameters},
    {"GetDistanceFactor", 2, L3D_GetDistanceFactor},
    {"GetDopplerFactor", 2, L3D_GetDopplerFactor},
    {"GetOrientation", 3, L3D_GetOrientation},
    {"GetPosition", 2, L3D_GetPosition},
    {"GetRolloffFactor", 2, L3D_GetRolloffFactor},
    {"GetVelocity", 2, L3D_GetVelocity},
    {"SetAllParameters", 3, L3D_SetAllParameters},
    {"SetDistanceFactor", 3, L3D_SetDistanceFactor},
    {"SetDopplerFactor", 3, L3D_SetDopplerFactor},
    {"SetOrientation", 8, L3D_SetOrientation},
    {"SetPosition", 5, L3D_SetPosition},
    {"SetRolloffFactor", 3, L3D_SetRolloffFactor},
    {"SetVelocity", 5, L3D_SetVelocity},
    {"CommitDeferredSettings", 1, L3D_CommitDeferredSettings},
};

// ===========================================================================
// IDirectSoundNotify - a view on the buffer object.
// ===========================================================================
void Notify_SetNotificationPositions(X86 *c) {
    ComObj *b = this_buffer(c);
    uint32_t n = arg(c, 1), list = arg(c, 2);
    if (!b) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    if (n && (!list || !gm_valid(list, n * 8))) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    // DirectSound refuses a change while the buffer is playing, because the
    // positions it is being asked to forget may be the ones it is between.
    if (n && b->playing) {
        com_ret(c, DSERR_INVALIDCALL);
        return;
    }
    b->notify_count = n;

    NotifyRecord *rec = notify_for(b->id, true);
    rec->pos.clear();
    rec->tracking = false;
    // DSBPOSITIONNOTIFY is {dwOffset, hEventNotify}, eight bytes each.
    for (uint32_t i = 0; i < n; ++i) {
        NotifyPos p;
        p.offset = rd32(list + 8u * i);
        p.event = rd32(list + 8u * i + 4);
        if (p.offset != DSBPN_OFFSETSTOP && b->buf_bytes && p.offset >= b->buf_bytes) {
            LOGW("dsound: notification position %u is past the end of a "
                 "%u-byte buffer; ignoring it",
                 p.offset, b->buf_bytes);
            continue;
        }
        rec->pos.push_back(p);
    }
    LOGV("dsound: buffer %u takes %u notification positions", b->id, n);
    com_ret(c, DS_OK);
}

const ComMethod g_dsnotify[] = {
    {"QueryInterface", 3, com_QueryInterface},
    {"AddRef", 1, com_AddRef},
    {"Release", 1, com_Release},
    {"SetNotificationPositions", 3, Notify_SetNotificationPositions},
};

// ===========================================================================
// IDirectSound
// ===========================================================================
ComObj *new_primary_buffer(uint32_t rate, uint32_t bits, uint32_t channels, uint32_t flags) {
    ComObj *b = com_new(K_DSBUFFER);
    b->buf_flags = flags;
    b->is_3d = (flags & DSBCAPS_CTRL3D) != 0;
    b->is_primary_buffer = true;
    b->rate = rate;
    b->bits = bits;
    b->nchannels = channels;
    b->block_align = (uint16_t)(channels * (bits / 8));
    return b;
}

ComObj *new_dsound_object(ComIface iface) {
    ComObj *ds = com_new(K_DSOUND);
    if (!com_view(ds, iface)) {
        com_release(ds);
        return nullptr;
    }
    return ds;
}

void DS_CreateSoundBuffer(X86 *c) {
    ComObj *ds = this_dsound(c);
    uint32_t desc = arg(c, 1), out = arg(c, 2), outer = arg(c, 3);
    if (!ds || !out) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    com_out_ptr(out, 0);
    if (outer) {
        com_ret(c, CLASS_E_NOAGGREGATION);
        return;
    }
    if (!desc || !gm_valid(desc, DSBUFFERDESC_SIZE)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }

    uint32_t flags = rd32(desc + DSBD_OFF_dwFlags);
    uint32_t bytes = rd32(desc + DSBD_OFF_dwBufferBytes);
    uint32_t wfx = rd32(desc + DSBD_OFF_lpwfxFormat);
    bool primary = (flags & DSBCAPS_PRIMARYBUFFER) != 0;

    // DirectSound requires a format for a secondary buffer and forbids one for
    // the primary, and a secondary buffer must have a non-zero size.
    if (primary) {
        if (bytes || wfx) {
            com_ret(c, DSERR_INVALIDPARAM);
            return;
        }
    } else {
        if (!bytes || !wfx) {
            com_ret(c, DSERR_INVALIDPARAM);
            return;
        }
        if (bytes > 64u * 1024 * 1024) {
            com_ret(c, DSERR_INVALIDPARAM);
            return;
        }
    }

    ComObj *b = primary ? new_primary_buffer(22050, 16, 2, flags) : com_new(K_DSBUFFER);
    if (!b) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    if (!primary) {
        b->buf_flags = flags;
        b->is_3d = (flags & DSBCAPS_CTRL3D) != 0;
        read_wave_format(b, wfx);
        b->buf_bytes = bytes;
        b->buf_pixels = heap_alloc(bytes, true, 16);
        pcm_retain(b->buf_pixels);
        if (!b->buf_pixels) {
            com_release(b);
            LOGW("dsound: out of guest memory for a %u-byte sound buffer", bytes);
            com_ret(c, E_OUTOFMEMORY);
            return;
        }
    }
    uint32_t view = com_view(b, IF_DSBUFFER);
    if (!view) {
        com_release(b);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_out_ptr(out, view);
    LOGV("dsound: created a %s buffer of %u bytes (flags %08x)", primary ? "primary" : "secondary",
         bytes, flags);
    com_ret(c, DS_OK);
}

// The host mixer converts every source to the output rate, so the whole
// DirectSound secondary frequency range is honored. The buffer formats the
// mixer accepts are 8/16-bit mono/stereo for both the primary and secondary
// buffers; mixing is software, so no hardware buffer or memory counts are
// reported. The engine reads dwFlags and the sample-rate fields when it builds
// its audio-capabilities string (FUN_00545e30).
void DS_GetCaps(X86 *c) {
    uint32_t out = arg(c, 1);
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    uint32_t size = rd32(out);
    if (size < 8 || size > DSCAPS_SIZE || !gm_valid(out, size)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    gm_zero(out + 4, size - 4);
    wr32(out + DSCAPS_OFF_dwFlags,
         DSCAPS_PRIMARYMONO | DSCAPS_PRIMARYSTEREO | DSCAPS_PRIMARY8BIT | DSCAPS_PRIMARY16BIT |
             DSCAPS_CONTINUOUSRATE | DSCAPS_CERTIFIED | DSCAPS_SECONDARYMONO |
             DSCAPS_SECONDARYSTEREO | DSCAPS_SECONDARY8BIT | DSCAPS_SECONDARY16BIT);
    wr32(out + DSCAPS_OFF_dwMinSecondarySampleRate, 100);
    wr32(out + DSCAPS_OFF_dwMaxSecondarySampleRate, 200000);
    wr32(out + DSCAPS_OFF_dwPrimaryBuffers, 1);
    com_ret(c, DS_OK);
}

void DS_DuplicateSoundBuffer(X86 *c) {
    ComObj *ds = this_dsound(c);
    uint32_t src_ptr = arg(c, 1), out = arg(c, 2);
    ComObj *src = src_ptr ? com_this(src_ptr) : nullptr;
    if (!ds || !src || src->kind != K_DSBUFFER || !out) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    if (src->is_primary_buffer) {
        com_ret(c, DSERR_INVALIDCALL);
        return;
    }
    ComObj *b = com_new(K_DSBUFFER);
    b->buf_flags = src->buf_flags;
    b->buf_bytes = src->buf_bytes;
    b->rate = src->rate;
    b->nchannels = src->nchannels;
    b->bits = src->bits;
    b->block_align = src->block_align;
    b->volume = src->volume;
    b->pan = src->pan;
    // A duplicate shares the original's sample data in DirectSound. It takes
    // its own reference, so releasing either buffer in either order leaves the
    // samples valid for whichever one is still alive.
    b->buf_pixels = src->buf_pixels;
    b->buf_bytes = src->buf_bytes;
    pcm_retain(b->buf_pixels);
    uint32_t view = com_view(b, IF_DSBUFFER);
    if (!view) {
        com_release(b);
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    com_out_ptr(out, view);
    com_ret(c, DS_OK);
}

void DS_SetCooperativeLevel(X86 *c) {
    ComObj *ds = this_dsound(c);
    if (!ds) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    ds->ds_hwnd = arg(c, 1);
    ds->ds_coop = arg(c, 2);
    com_ret(c, DS_OK);
}

void DS_Compact(X86 *c) {
    com_ret(c, DS_OK);
}

void DS_GetSpeakerConfig(X86 *c) {
    uint32_t out = arg(c, 1);
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    wr32(out, 2); // DSSPEAKER_STEREO
    com_ret(c, DS_OK);
}

void DS_SetSpeakerConfig(X86 *c) {
    com_ret(c, DS_OK);
}
// Initialize(lpGuid): an object from DirectSoundCreate is already initialised
// and one from CoCreateInstance is not; both are the same host object here,
// and the default device is the only device, so it succeeds either way.
void DS_Initialize(X86 *c) {
    com_ret(c, DS_OK);
}

// IDirectSound8::VerifyCertification. The host is a compatibility shim, not a
// WHQL-certified driver, but GetCaps already reports DSCAPS_CERTIFIED and the
// original hardware was certified; answer consistently so the game does not
// disable sound on a mismatch. DS_CERTIFIED is 0, DS_UNCERTIFIED is 1.
void DS_VerifyCertification(X86 *c) {
    uint32_t out = arg(c, 1);
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    wr32(out, 0);
    com_ret(c, DS_OK);
}

const ComMethod g_dsound[] = {
    {"QueryInterface", 3, com_QueryInterface},
    {"AddRef", 1, com_AddRef},
    {"Release", 1, com_Release},
    {"CreateSoundBuffer", 4, DS_CreateSoundBuffer},
    {"GetCaps", 2, DS_GetCaps},
    {"DuplicateSoundBuffer", 3, DS_DuplicateSoundBuffer},
    {"SetCooperativeLevel", 3, DS_SetCooperativeLevel},
    {"Compact", 1, DS_Compact},
    {"GetSpeakerConfig", 2, DS_GetSpeakerConfig},
    {"SetSpeakerConfig", 2, DS_SetSpeakerConfig},
    {"Initialize", 2, DS_Initialize},
};

// IDirectSound8 is IDirectSound plus VerifyCertification; the same host object
// answers both views. No method is left unknown: anything the interface does
// not implement is refused by the shared handlers above.
const ComMethod g_dsound8[] = {
    {"QueryInterface", 3, com_QueryInterface},
    {"AddRef", 1, com_AddRef},
    {"Release", 1, com_Release},
    {"CreateSoundBuffer", 4, DS_CreateSoundBuffer},
    {"GetCaps", 2, DS_GetCaps},
    {"DuplicateSoundBuffer", 3, DS_DuplicateSoundBuffer},
    {"SetCooperativeLevel", 3, DS_SetCooperativeLevel},
    {"Compact", 1, DS_Compact},
    {"GetSpeakerConfig", 2, DS_GetSpeakerConfig},
    {"SetSpeakerConfig", 2, DS_SetSpeakerConfig},
    {"Initialize", 2, DS_Initialize},
    {"VerifyCertification", 2, DS_VerifyCertification},
};

// ===========================================================================
// DSOUND.dll exports
// ===========================================================================
// DirectSoundCreate(lpGuid, ppDS, pUnkOuter). The EXE imports it by ordinal 1,
// which the loader names "ord1"; both names are registered so the IAT entry
// and GetProcAddress agree. DirectSoundCreate8 (ordinal 11) makes the same
// host object through the IDirectSound8 view.
static void create_dsound_device(X86 *c, ComIface iface) {
    uint32_t guid = arg(c, 0), out = arg(c, 1), outer = arg(c, 2);
    if (!out || !gm_valid(out, 4)) {
        com_ret(c, DSERR_INVALIDPARAM);
        return;
    }
    wr32(out, 0);
    if (outer) {
        com_ret(c, CLASS_E_NOAGGREGATION);
        return;
    }
    // The primary-driver alias and the enumerated software device share the
    // host mixer. Reject identities we did not enumerate.
    if (guid && (!gm_valid(guid, 16) || memcmp(gm_ptr(guid), GUID_RecompPlayback_, 16) != 0)) {
        LOGW("dsound: create asked for device GUID %08x, but it was not enumerated", guid);
        com_ret(c, DSERR_NODRIVER);
        return;
    }
    ComObj *ds = new_dsound_object(iface);
    if (!ds) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    uint32_t view = com_view(ds, iface);
    wr32(out, view);
    LOGV("dsound: create -> %08x", view);
    com_ret(c, DS_OK);
}

void DirectSoundCreate(X86 *c) {
    create_dsound_device(c, IF_DSOUND);
}
void DirectSoundCreate8(X86 *c) {
    create_dsound_device(c, IF_DSOUND8);
}

// Enumerate the primary-driver alias followed by the concrete software device.
// Callers may skip the NULL-GUID alias when choosing a playback device. All
// callback pointers refer to guest storage valid until enumeration returns.
void DirectSoundEnumerateA(X86 *c) {
    uint32_t cb = arg(c, 0), ctx = arg(c, 1);
    if (!cb) {
        set_eax(c, DSERR_INVALIDPARAM);
        return;
    }
    uint32_t strs = heap_alloc(144, false, 16);
    if (!strs) {
        set_eax(c, E_OUTOFMEMORY);
        return;
    }
    gm_put_str(strs, "Primary Sound Driver", 64);
    gm_put_str(strs + 64, "dsound.dll", 64);
    if (guest_call(c, cb, 0, strs, strs + 64, ctx)) {
        memcpy(gm_ptr(strs + 128), GUID_RecompPlayback_, 16);
        gm_put_str(strs, "Recomp Software Playback", 64);
        guest_call(c, cb, strs + 128, strs, strs + 64, ctx);
    }
    heap_free(strs);
    set_eax(c, DS_OK);
}

// DirectSoundCaptureEnumerateA(callback, context). The bridge provides no
// capture device, so the callback is never invoked and the call reports
// success with an empty list, the same result as a machine with no input.
void DirectSoundCaptureEnumerateA(X86 *c) {
    if (!arg(c, 0)) {
        set_eax(c, DSERR_INVALIDPARAM);
        return;
    }
    set_eax(c, DS_OK);
}

// DirectSoundCaptureCreate(lpGuid, ppCapture, pUnkOuter). Capture is not
// modelled; the caller learns there is no driver so it can disable capture
// rather than receive a NULL object with a success code.
void DirectSoundCaptureCreate(X86 *c) {
    uint32_t out = arg(c, 1);
    if (out && gm_valid(out, 4))
        wr32(out, 0);
    set_eax(c, DSERR_NODRIVER);
}

// GetDeviceID(lpGuidSrc, lpGuidDest). The bridge has one playback identity and
// no capture device, so both playback default-device IDs map onto the
// enumerated playback GUID; every other GUID is returned unchanged.
void DirectSoundGetDeviceID(X86 *c) {
    uint32_t src = arg(c, 0), dst = arg(c, 1);
    if (!dst || !gm_valid(dst, 16)) {
        set_eax(c, DSERR_INVALIDPARAM);
        return;
    }
    if (!src) {
        memcpy(gm_ptr(dst), GUID_RecompPlayback_, 16);
        set_eax(c, DS_OK);
        return;
    }
    if (!gm_valid(src, 16)) {
        set_eax(c, DSERR_INVALIDPARAM);
        return;
    }
    // DSDEVID_Default{Playback,VoicePlayback} share the suffix
    // {9C6D-47ED-AAF1-4DDA8F2B5C03} and differ only in DEF00000/DEF00002.
    static const uint8_t suffix[12] = {0x6d, 0x9c, 0xed, 0x47, 0xaa, 0xf1,
                                       0x4d, 0xda, 0x8f, 0x2b, 0x5c, 0x03};
    uint32_t id = rd32(src);
    if ((id == 0xdef00000u || id == 0xdef00002u) && memcmp(gm_ptr(src) + 4, suffix, 12) == 0)
        memcpy(gm_ptr(dst), GUID_RecompPlayback_, 16);
    else
        memcpy(gm_ptr(dst), gm_ptr(src), 16);
    set_eax(c, DS_OK);
}

// CLSID_DirectSound and CLSID_DirectSound8, for a game that creates its
// DirectSound through CoCreateInstance rather than DirectSoundCreate(8). All
// make the same host object.
static const uint8_t CLSID_DirectSound_[16] =
    IID_BYTES(0x47D4D946, 0x62E8, 0x11CF, 0x93, 0xBC, 0x44, 0x45, 0x53, 0x54, 0x00, 0x00);
static const uint8_t CLSID_DirectSound8_[16] =
    IID_BYTES(0x3901CC3F, 0x84B5, 0x4FA4, 0xBA, 0x35, 0xAA, 0x81, 0x72, 0xB8, 0xA0, 0x9B);

static ComObj *dsound_create() {
    return com_new(K_DSOUND);
}

const ImportShim g_dsound_exports[] = {
    {"DSOUND.dll", "ord1", 3, DirectSoundCreate},
    {"DSOUND.dll", "DirectSoundCreate", 3, DirectSoundCreate},
    {"DSOUND.dll", "ord2", 2, DirectSoundEnumerateA},
    {"DSOUND.dll", "DirectSoundEnumerateA", 2, DirectSoundEnumerateA},
    {"DSOUND.dll", "ord11", 3, DirectSoundCreate8},
    {"DSOUND.dll", "DirectSoundCreate8", 3, DirectSoundCreate8},
    {"DSOUND.dll", "ord6", 3, DirectSoundCaptureCreate},
    {"DSOUND.dll", "DirectSoundCaptureCreate", 3, DirectSoundCaptureCreate},
    {"DSOUND.dll", "ord7", 2, DirectSoundCaptureEnumerateA},
    {"DSOUND.dll", "DirectSoundCaptureEnumerateA", 2, DirectSoundCaptureEnumerateA},
    {"DSOUND.dll", "ord9", 2, DirectSoundGetDeviceID},
    {"DSOUND.dll", "GetDeviceID", 2, DirectSoundGetDeviceID},
};

// A DirectSound object also answers to IDirectSound3DListener, and a buffer to
// IDirectSound3DBuffer, IDirectSoundNotify and IDirectSound3DListener (the
// primary buffer is where DirectSound exposes the listener). Each is the same
// host object seen through another interface, which the view table handles.
ComObj *dsound_qi(ComObj *self, ComIface want) {
    if (want == IF_DS3DLISTENER)
        return self;
    return nullptr;
}
ComObj *dsbuffer_qi(ComObj *self, ComIface want) {
    // The primary buffer also exposes the 3D listener, as real DirectSound does.
    // The listener view is global bookkeeping (position/orientation/factors),
    // and its position/orientation now drive the mix for the 3D buffers.
    if (want == IF_DS3DBUFFER) {
        self->is_3d = true; // a buffer that has a 3D interface is a 3D buffer
        return self;
    }
    if (want == IF_DSNOTIFY || want == IF_DS3DLISTENER || want == IF_DSBUFFER8)
        return self;
    return nullptr;
}

void dsbuffer_destroy(ComObj *b) {
    for (size_t i = 0; i < notifies().size(); ++i) {
        if (notifies()[i].obj_id != b->id)
            continue;
        notifies().erase(notifies().begin() + (ptrdiff_t)i);
        break;
    }
    stream_drop(b->id);
    if (b->playing && b->channel >= 0)
        host_audio_stop(b->channel);
    if (b->channel >= 0) {
        dx_free_audio_channel(b->channel);
        b->channel = -1;
    }
    // Every holder of the sample memory, original and duplicates alike, drops
    // one reference; the last one out frees it.
    pcm_release(b->buf_pixels);
    b->buf_pixels = 0;
}

} // namespace

ComObj *dsound_new_device() {
    return new_dsound_object(IF_DSOUND);
}

ComObj *dsound_new_primary_buffer(uint32_t rate, uint32_t bits, uint32_t channels, uint32_t flags) {
    return new_primary_buffer(rate, bits, channels, flags);
}

void dsound_reset() {
    // The sample blocks lived in the arena mem_init discarded, so the
    // reference table must not outlive it. So did the guest event handles the
    // notification records name.
    pcm_refs().clear();
    notifies().clear();
    streams().clear();
    announced().clear();
    for (int i = 0; i < 3; ++i) {
        g_listener[i] = 0.0f;
        g_listener[9 + i] = 0.0f;
    }
    g_listener[3] = 0.0f;
    g_listener[4] = 0.0f;
    g_listener[5] = 1.0f;
    g_listener[6] = 0.0f;
    g_listener[7] = 1.0f;
    g_listener[8] = 0.0f;
    g_distance_factor = g_doppler_factor = g_rolloff_factor = 1.0f;
}

// Called from QSWaveMixPump, which the game runs every frame. The streaming
// worker is parked in WaitForMultipleObjects and cannot service itself, and
// the main thread has no reason to touch DirectSound between refills, so this
// is the tick that keeps a streamed buffer moving.
void dsound_pump() {
    service_notifications();
}

#include "com_host_slots.inc"

COM_HOST_SLOTS(dsound_IDirectSound, IF_DSOUND, g_dsound, true)
COM_HOST_SLOTS(dsound_IDirectSound8, IF_DSOUND8, g_dsound8, true)
COM_HOST_SLOTS(dsound_IDirectSoundBuffer, IF_DSBUFFER, g_dsbuffer, true)
COM_HOST_SLOTS(dsound_IDirectSoundBuffer8, IF_DSBUFFER8, g_dsbuffer8, true)
COM_HOST_SLOTS(dsound_IDirectSound3DBuffer, IF_DS3DBUFFER, g_ds3dbuffer, true)
COM_HOST_SLOTS(dsound_IDirectSound3DListener, IF_DS3DLISTENER, g_ds3dlistener, true)
COM_HOST_SLOTS(dsound_IDirectSoundNotify, IF_DSNOTIFY, g_dsnotify, true)

void dsound_register() {
    static bool done = false;
    if (done)
        return;
    done = true;

    dsound_IDirectSound_host::define("DSOUND.dll", "IDirectSound");
    dsound_IDirectSound8_host::define("DSOUND.dll", "IDirectSound8");
    dsound_IDirectSoundBuffer_host::define("DSOUND.dll", "IDirectSoundBuffer");
    dsound_IDirectSoundBuffer8_host::define("DSOUND.dll", "IDirectSoundBuffer8");
    dsound_IDirectSound3DBuffer_host::define("DSOUND.dll", "IDirectSound3DBuffer");
    dsound_IDirectSound3DListener_host::define("DSOUND.dll", "IDirectSound3DListener");
    dsound_IDirectSoundNotify_host::define("DSOUND.dll", "IDirectSoundNotify");

    com_bind(IF_DSOUND, K_DSOUND);
    com_bind(IF_DSOUND8, K_DSOUND);
    com_bind(IF_DS3DLISTENER, K_DSOUND);
    com_bind(IF_DSBUFFER, K_DSBUFFER);
    com_bind(IF_DSBUFFER8, K_DSBUFFER);
    com_bind(IF_DS3DBUFFER, K_DSBUFFER);
    com_bind(IF_DS3DLISTENER, K_DSBUFFER); // the primary buffer exposes the listener
    com_bind(IF_DSNOTIFY, K_DSBUFFER);

    com_register_iid(IF_DSOUND, IID_IDirectSound_);
    com_register_iid(IF_DSOUND8, IID_IDirectSound8_);
    com_register_iid(IF_DSBUFFER, IID_IDirectSoundBuffer_);
    com_register_iid(IF_DSBUFFER8, IID_IDirectSoundBuffer8_);
    com_register_iid(IF_DS3DBUFFER, IID_IDirectSound3DBuffer_);
    com_register_iid(IF_DS3DLISTENER, IID_IDirectSound3DListener_);
    com_register_iid(IF_DSNOTIFY, IID_IDirectSoundNotify_);

    com_set_destructor(K_DSBUFFER, dsbuffer_destroy);
    com_set_qi_hook(K_DSOUND, dsound_qi);
    com_set_qi_hook(K_DSBUFFER, dsbuffer_qi);

    com_register_class(CLSID_DirectSound_, "DirectSound", IF_DSOUND, dsound_create);
    com_register_class(CLSID_DirectSound8_, "DirectSound8", IF_DSOUND8, dsound_create);
    imports_register(g_dsound_exports, std::size(g_dsound_exports));
}
