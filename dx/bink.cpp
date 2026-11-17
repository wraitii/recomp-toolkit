// Bink video imports decode through FFmpeg when available. A build without
// it returns a finished record; Smacker remains refused. All decoder state
// stays on the host, keyed by a 32-bit guest record address.
#include "dx.h"
#include "com.h"
#include "host_api.h"
#include "video_frame.h"
#include "../runtime/imports.h"
#include "../runtime/memory.h"
#include "../runtime/win32.h"
#include "../runtime/display_seam.h"
#include "../platform/os.h"

#include <algorithm>
#include <cmath>
#include <errno.h>
#include <string.h>
#include <iterator>

#ifdef RECOMP_HAVE_FFMPEG
extern "C" {
#include <libavformat/avformat.h>
#include <libavcodec/avcodec.h>
}
#include <deque>
#include <map>
#include <memory>
#include <vector>
#endif

namespace {

// binkw32!BinkOpen allocates 0x38c bytes and copies that many from its local
// BINK struct (0x3001286a/0x30012890); RT3 reads fields up to +0x2a0.
constexpr uint32_t BINK_RECORD_BYTES = 0x38c;
constexpr uint32_t BINK_SUMMARY_BYTES = 0x7c;
// _BinkGetRealtime@12 writes this many bytes: FrameNum, FrameRate, FrameRateDiv,
// Frames, FramesTime and the ten stage/buffer/rate fields the shipped Bink 1.0
// DLL fills (disassembled at binkw32.dll 0x30006380).
constexpr uint32_t BINK_REALTIME_BYTES = 0x38;
constexpr uint32_t BINK_FILE_HANDLE = 0x00800000;
constexpr uint32_t BINK_FROM_MEMORY = 0x04000000;
// _BinkOpen@8 consumes the size from _BinkSetIOSize@4 only under this flag.
constexpr uint32_t BINK_IO_SIZE = 0x01000000;
constexpr uint32_t BINK_DEFAULT_IO_SIZE = 64 * 1024;
constexpr uint32_t BINK_MIN_IO_SIZE = 4096;
uint32_t g_io_size = 0;
uint32_t g_error_string = 0;
char g_error[512] = {};

void ret0(X86 *c) {
    set_eax(c, 0);
}
void ret1(X86 *c) {
    set_eax(c, 1);
}

bool video_error(const char *text) {
    snprintf(g_error, sizeof g_error, "%s", text);
    LOGW("bink: %s", g_error);
    return false;
}

uint32_t take_io_size(uint32_t flags) {
    if (!(flags & BINK_IO_SIZE))
        return 0;
    uint32_t io_size = g_io_size;
    g_io_size = 0;
    return io_size;
}

bool check_open_flags(uint32_t flags) {
    if (flags & BINK_FROM_MEMORY)
        return video_error("memory-resident video is not supported");
    uint32_t ignored = flags & ~(BINK_FILE_HANDLE | BINK_FROM_MEMORY | BINK_IO_SIZE);
    if (ignored)
        LOGV("bink: ignoring open flags %08x", ignored);
    return true;
}

#ifdef RECOMP_HAVE_FFMPEG
bool decoder_error(const char *operation, int code) {
    char detail[AV_ERROR_MAX_STRING_SIZE], message[512];
    av_strerror(code, detail, sizeof detail);
    snprintf(message, sizeof message, "%s: %s", operation, detail);
    return video_error(message);
}

// Only the guest baton holder touches a player. Audio service reads ahead
// without advancing the visible frame: compressed video packets are retained
// for DoFrame, and the host mixer copies every submitted PCM chunk.
struct BinkPlayer {
    AVFormatContext *input = nullptr;
    AVIOContext *io = nullptr;
    int file_fd = -1;
    int64_t file_start = 0, file_length = 0, file_position = 0;
    AVCodecContext *video = nullptr, *audio = nullptr;
    AVFrame *frame = nullptr, *audio_frame = nullptr;
    std::deque<AVPacket *> video_packets;
    std::vector<int16_t> pending;
    size_t pending_pos = 0;
    int video_index = -1, audio_index = -1;
    int32_t channel = -1;
    AVRational fps{};
    uint32_t t0 = 0, paused_at = 0, count = 0, current = 1, displayed = 0;
    bool paused = false;
    bool eof = false, flushed = false, failed = false;
    bool have_frame = false, audio_started = false, audio_unavailable = false;
    // BinkSetSoundOnOff: the guest movie player enables/disables the stream's
    // audio. Kept here because audio is host-rendered, not mixed by the guest.
    bool sound_on = true;
    // BinkSetVolume on track 0, in millibels for host_audio_set_volume.
    int32_t volume_mb = 0;
    // BinkGetRealtime accounting. Bink keeps per-frame accumulators that this
    // shim does not reproduce; these are host-clock measurements (ms) of the
    // work the shim actually does, plus the compressed bytes it has read.
    uint64_t video_ms = 0, audio_ms = 0, read_ms = 0, blit_ms = 0;
    uint64_t bytes_read = 0, total_bytes = 0;

    // Playback wall time in milliseconds, pause-adjusted like BinkWait.
    uint32_t elapsed_millis() const {
        return (paused ? paused_at : host_millis()) - t0;
    }

    ~BinkPlayer() {
        if (channel >= 0) {
            host_audio_stop(channel);
            dx_free_audio_channel(channel);
        }
        for (AVPacket *packet : video_packets)
            av_packet_free(&packet);
        av_frame_free(&frame);
        av_frame_free(&audio_frame);
        avcodec_free_context(&video);
        avcodec_free_context(&audio);
        avformat_close_input(&input);
        // Custom I/O survives close_input, including a failed open. FFmpeg
        // may replace the original buffer, so free the context's current one.
        if (io) {
            av_freep(&io->buffer);
            avio_context_free(&io);
        }
        if (file_fd >= 0)
            os_fd_close(file_fd);
    }
};
std::map<uint32_t, std::unique_ptr<BinkPlayer>> g_players;

BinkPlayer *player_for(uint32_t rec) {
    auto it = g_players.find(rec);
    return it == g_players.end() ? nullptr : it->second.get();
}

// Expose only the host-file window beginning at the guest's saved position.
// All callbacks use the player's reopened descriptor, never the guest's fd.
int read_file_window(void *opaque, uint8_t *buffer, int bytes) {
    auto &p = *static_cast<BinkPlayer *>(opaque);
    if (bytes <= 0)
        return AVERROR(EINVAL);
    size_t wanted = (size_t)std::min<int64_t>(bytes, p.file_length - p.file_position);
    if (!wanted)
        return AVERROR_EOF;
    int64_t got;
    do {
        got = os_fd_read(p.file_fd, buffer, wanted);
    } while (got < 0 && errno == EINTR);
    if (got < 0)
        return AVERROR(errno);
    if (!got)
        return AVERROR_EOF;
    p.file_position += got;
    return (int)got;
}

int64_t seek_file_window(void *opaque, int64_t offset, int whence) {
    auto &p = *static_cast<BinkPlayer *>(opaque);
    whence &= ~AVSEEK_FORCE;
    if (whence == AVSEEK_SIZE)
        return p.file_length;
    int64_t base;
    switch (whence) {
    case SEEK_SET:
        base = 0;
        break;
    case SEEK_CUR:
        base = p.file_position;
        break;
    case SEEK_END:
        base = p.file_length;
        break;
    default:
        return AVERROR(EINVAL);
    }
    // Check the relative addition before computing a host offset; seeking
    // before the stream or beyond the host file must not escape the window.
    if (offset < -base || offset > p.file_length - base)
        return AVERROR(EINVAL);
    int64_t position = base + offset;
    if (os_fd_seek(p.file_fd, p.file_start + position, OS_SEEK_SET) < 0)
        return AVERROR(errno);
    p.file_position = position;
    return position;
}

// Byte zero is the handle's current offset, and AVSEEK_SIZE reports the rest
// of the host file. Bink's own header bounds its frames, so trailing archive
// data is harmless. The player owns every allocation even if open fails.
bool open_file_window(BinkPlayer &p, const std::string &path, int64_t offset, uint32_t io_size) {
    p.file_fd = os_fd_open(path.c_str(), OS_O_RDONLY);
    if (p.file_fd < 0)
        return decoder_error("open video file", AVERROR(errno));
    OsStat st{};
    if (os_fd_stat(p.file_fd, &st) < 0)
        return decoder_error("query video file size", AVERROR(errno));
    if (offset < 0 || st.size > INT64_MAX || (uint64_t)offset > st.size)
        return video_error("video file offset is outside the host file");
    if (os_fd_seek(p.file_fd, offset, OS_SEEK_SET) < 0)
        return decoder_error("seek video file", AVERROR(errno));
    p.file_start = offset;
    p.file_length = (int64_t)st.size - offset;
    p.input = avformat_alloc_context();
    if (!p.input)
        return video_error("cannot allocate video input");
    uint32_t buffer_bytes = io_size ? io_size : BINK_DEFAULT_IO_SIZE;
    if (buffer_bytes < BINK_MIN_IO_SIZE)
        buffer_bytes = BINK_MIN_IO_SIZE;
    auto *buffer = static_cast<uint8_t *>(av_malloc(buffer_bytes));
    if (!buffer)
        return video_error("cannot allocate video I/O buffer");
    p.io = avio_alloc_context(buffer, buffer_bytes, 0, &p, read_file_window, nullptr,
                              seek_file_window);
    if (!p.io) {
        av_freep(&buffer);
        return video_error("cannot allocate video I/O context");
    }
    p.input->pb = p.io;
    p.input->flags |= AVFMT_FLAG_CUSTOM_IO;
    int rc = avformat_open_input(&p.input, nullptr, nullptr, nullptr);
    return rc < 0 ? decoder_error("open input", rc) : true;
}

// The imported record has two observed layouts. Both pairs describe the
// same 1-based counter; host pointers never enter these guest fields.
// The guest's end test is [+0x10] == [+0x8]: +0x8 is Frames, +0x10 the last
// displayed frame, +0xc the frame to decode next, +0x14 FrameRate and +0x18
// FrameRateDiv (binkw32 0x30011f97/0x3001203f keeps +0x10 one behind +0xc).
void write_frame_count(uint32_t rec, const BinkPlayer &p) {
    wr32(rec + 0x08, p.count);
    wr32(rec + 0x0c, p.current);
    wr32(rec + 0x10, p.displayed);
    wr32(rec + 0x14, (uint32_t)p.fps.num);
    wr32(rec + 0x18, (uint32_t)p.fps.den);
}

bool open_decoder(AVFormatContext *input, int index, AVCodecContext **context) {
    const AVCodec *codec = avcodec_find_decoder(input->streams[index]->codecpar->codec_id);
    if (!codec)
        return video_error("decoder is unavailable");
    *context = avcodec_alloc_context3(codec);
    if (!*context)
        return video_error("cannot allocate decoder");
    int rc = avcodec_parameters_to_context(*context, input->streams[index]->codecpar);
    if (rc < 0)
        return decoder_error("copy codec parameters", rc);
    rc = avcodec_open2(*context, codec, nullptr);
    return rc < 0 ? decoder_error("open decoder", rc) : true;
}

// Bink audio is float PCM, packed or planar. Clamp before conversion so
// full-scale peaks and non-finite decoder output cannot overflow s16.
bool decode_audio(BinkPlayer &p, const AVPacket *packet) {
    int rc = avcodec_send_packet(p.audio, packet);
    if (rc < 0)
        return decoder_error("send audio packet", rc);
    while ((rc = avcodec_receive_frame(p.audio, p.audio_frame)) >= 0) {
        const AVFrame &f = *p.audio_frame;
        if (f.format != AV_SAMPLE_FMT_FLT && f.format != AV_SAMPLE_FMT_FLTP)
            return video_error("unsupported audio sample format (expected float PCM)");
        int channels = f.ch_layout.nb_channels;
        if (channels != p.audio->ch_layout.nb_channels || f.sample_rate != p.audio->sample_rate)
            return video_error("audio format changed during playback");
        if (!p.audio_unavailable) {
            for (int i = 0; i < f.nb_samples; ++i)
                for (int ch = 0; ch < channels; ++ch) {
                    const float *samples = reinterpret_cast<const float *>(
                        f.extended_data[f.format == AV_SAMPLE_FMT_FLTP ? ch : 0]);
                    float value = samples[f.format == AV_SAMPLE_FMT_FLTP ? i : i * channels + ch];
                    if (!std::isfinite(value))
                        value = 0;
                    int sample = (int)std::lround(std::clamp(value, -1.0f, 1.0f) * 32768.0f);
                    p.pending.push_back((int16_t)std::clamp(sample, -32768, 32767));
                }
        }
        av_frame_unref(p.audio_frame);
    }
    return rc == AVERROR(EAGAIN) || rc == AVERROR_EOF || decoder_error("receive audio frame", rc);
}

// Demux one packet, retaining video for DoFrame and decoding audio for the
// refill queue. EOF drains the audio decoder once; video drains in DoFrame.
bool read_packet(BinkPlayer &p) {
    if (p.eof)
        return true;
    AVPacket *packet = av_packet_alloc();
    if (!packet)
        return video_error("cannot allocate packet");
    uint32_t read_start = host_millis();
    int rc = av_read_frame(p.input, packet);
    p.read_ms += host_millis() - read_start;
    if (rc < 0) {
        av_packet_free(&packet);
        if (rc != AVERROR_EOF)
            return decoder_error("read packet", rc);
        p.eof = true;
        uint32_t drain_start = host_millis();
        bool drained = !p.audio || decode_audio(p, nullptr);
        p.audio_ms += host_millis() - drain_start;
        return drained;
    }
    p.bytes_read += (uint64_t)packet->size;
    if (packet->stream_index == p.video_index) {
        p.video_packets.push_back(packet);
        return true;
    }
    bool ok = true;
    if (packet->stream_index == p.audio_index) {
        uint32_t audio_start = host_millis();
        ok = decode_audio(p, packet);
        p.audio_ms += host_millis() - audio_start;
    }
    av_packet_free(&packet);
    return ok;
}

// Finds the streams, opens their decoders and reads the frame count and rate.
bool prepare_streams(BinkPlayer &p) {
    int rc = avformat_find_stream_info(p.input, nullptr);
    if (rc < 0)
        return decoder_error("find stream info", rc);
    // The real Bink reader reports its byte budget in the realtime record; the
    // custom I/O window knows its length and a regular file knows its size.
    int64_t total = avio_size(p.input->pb);
    p.total_bytes = total > 0 ? (uint64_t)total : (uint64_t)p.file_length;
    for (unsigned i = 0; i < p.input->nb_streams; ++i) {
        AVMediaType type = p.input->streams[i]->codecpar->codec_type;
        if (type == AVMEDIA_TYPE_VIDEO && p.video_index < 0)
            p.video_index = (int)i;
        if (type == AVMEDIA_TYPE_AUDIO && p.audio_index < 0)
            p.audio_index = (int)i;
    }
    if (p.video_index < 0)
        return video_error("input has no video stream");
    if (!open_decoder(p.input, p.video_index, &p.video) ||
        (p.audio_index >= 0 && !open_decoder(p.input, p.audio_index, &p.audio)))
        return false;
    AVStream *stream = p.input->streams[p.video_index];
    p.fps = stream->avg_frame_rate;
    if (p.fps.num <= 0 || p.fps.den <= 0)
        p.fps = stream->r_frame_rate;
    if (p.fps.num <= 0 || p.fps.den <= 0 || p.video->width <= 0 || p.video->height <= 0)
        return video_error("invalid video dimensions or frame rate");
    int64_t count = stream->nb_frames;
    if (count <= 0 && stream->duration > 0)
        count = av_rescale_q(stream->duration, stream->time_base, av_inv_q(p.fps));
    if (count <= 0 && p.input->duration > 0)
        count = av_rescale_q(p.input->duration, AVRational{1, AV_TIME_BASE}, av_inv_q(p.fps));
    if (count <= 0 || count > UINT32_MAX)
        return video_error("invalid video frame count");
    if (p.audio && (p.audio->sample_rate <= 0 || p.audio->ch_layout.nb_channels < 1 ||
                    p.audio->ch_layout.nb_channels > 2))
        return video_error("unsupported audio rate or channel count");
    p.frame = av_frame_alloc();
    p.audio_frame = av_frame_alloc();
    if (!p.frame || !p.audio_frame)
        return video_error("cannot allocate decoded frames");
    p.count = (uint32_t)count;
    return true;
}

void BinkOpen(X86 *c) {
    set_eax(c, 0);
    g_error[0] = 0;
    uint32_t name = arg(c, 0), flags = arg(c, 1);
    if (!check_open_flags(flags))
        return;
    uint32_t io_size = take_io_size(flags);
    auto p = std::make_unique<BinkPlayer>();
    std::string guest;
    if (flags & BINK_FILE_HANDLE) {
        std::string path;
        int64_t offset;
        if (!win32_file_handle_position(name, &path, &offset)) {
            video_error("invalid video file handle");
            return;
        }
        if (!open_file_window(*p, path, offset, io_size))
            return;
        guest = win32_guest_path(path);
        LOGV("bink: file handle %08x at offset %lld", name, (long long)offset);
    } else {
        if (!name || !gm_valid(name, 1)) {
            video_error("invalid video filename");
            return;
        }
        guest = gm_str(name);
        std::string path = win32_host_path_op(guest, WIN32_FILE_READ);
        if (path.empty()) {
            video_error("cannot resolve video filename");
            return;
        }
        if (!open_file_window(*p, path, 0, io_size))
            return;
    }
    if (!prepare_streams(*p))
        return;
    uint32_t rec = heap_alloc(BINK_RECORD_BYTES, true, 16);
    if (!rec) {
        video_error("cannot allocate video record");
        return;
    }
    memset(g_mem + rec, 0, BINK_RECORD_BYTES);
    wr32(rec, (uint32_t)p->video->width);
    wr32(rec + 4, (uint32_t)p->video->height);
    // binkw32 sets this when the open found audio tracks (0x300130d1); the
    // guest uses it to pick its frame-skip path.
    wr32(rec + 0x2a0, p->audio ? 1u : 0u);
    p->t0 = host_millis();
    write_frame_count(rec, *p);
    LOGW("bink: open \"%s\" -> %dx%d, %u frames, %d/%d fps, record %08x", guest.c_str(),
         p->video->width, p->video->height, p->count, p->fps.num, p->fps.den, rec);
    g_players.emplace(rec, std::move(p));
    set_eax(c, rec);
}

// Start with PCM, convert the shared channel to a stream, then append until
// a second is queued. Refused chunks stay pending for the next service call.
void service_audio(uint32_t rec, BinkPlayer &p) {
    if (p.paused || !p.audio || p.failed || p.audio_unavailable || !p.sound_on)
        return;
    uint32_t block = (uint32_t)p.audio->ch_layout.nb_channels * 2;
    uint32_t ahead = (uint32_t)p.audio->sample_rate * block;
    while (!p.audio_started || host_audio_queued_bytes(p.channel) < ahead) {
        if (p.pending_pos == p.pending.size()) {
            p.pending.clear();
            p.pending_pos = 0;
            while (p.pending.empty() && !p.eof && !p.failed)
                p.failed = !read_packet(p);
            if (p.failed || p.pending.empty())
                break;
        }
        uint32_t bytes = (uint32_t)(p.pending.size() - p.pending_pos) * 2;
        if (!p.audio_started) {
            p.channel = dx_alloc_audio_channel();
            if (p.channel >= 0) {
                HostAudioPlay play{};
                play.channel = p.channel;
                play.pcm = p.pending.data() + p.pending_pos;
                play.bytes = bytes;
                play.sample_rate = p.audio->sample_rate;
                play.channels = p.audio->ch_layout.nb_channels;
                play.bits = 16;
                host_audio_play(&play);
                p.audio_started = host_audio_stream(p.channel) >= 0;
                if (p.audio_started)
                    host_audio_set_volume(p.channel, p.volume_mb);
            }
            if (!p.audio_started) {
                if (p.channel >= 0) {
                    host_audio_stop(p.channel);
                    dx_free_audio_channel(p.channel);
                    p.channel = -1;
                }
                LOGW("bink: host audio streaming unavailable");
                p.audio_unavailable = true;
                p.pending.clear();
                p.pending_pos = 0;
                break;
            }
            p.pending_pos += bytes / 2;
        } else {
            bytes = std::min(bytes, ahead - host_audio_queued_bytes(p.channel));
            bytes -= bytes % block;
            if (!bytes)
                break;
            int32_t taken = host_audio_queue(p.channel, p.pending.data() + p.pending_pos, bytes);
            if (taken <= 0)
                break;
            p.pending_pos += (uint32_t)taken / 2;
        }
    }
    if (p.failed && rec) {
        p.displayed = p.count;
        write_frame_count(rec, p);
    }
}

// Decode exactly one visible frame. Audio prefetch may already have retained
// its compressed packet, but never changes the guest frame counter.
bool decode_frame(BinkPlayer &p, uint32_t rec) {
    p.have_frame = false;
    uint32_t decode_start = host_millis();
    for (;;) {
        int rc = avcodec_receive_frame(p.video, p.frame);
        if (rc >= 0) {
            if (p.frame->format != AV_PIX_FMT_YUV420P) {
                p.failed = !video_error("unsupported video pixel format (expected YUV420P)");
                break;
            }
            if (p.frame->width != p.video->width || p.frame->height != p.video->height) {
                p.failed = !video_error("video dimensions changed during playback");
                break;
            }
            p.have_frame = true;
            p.video_ms += host_millis() - decode_start;
            service_audio(rec, p);
            return true;
        }
        if (rc != AVERROR(EAGAIN)) {
            p.failed = !decoder_error("receive video frame", rc);
            break;
        }
        while (p.video_packets.empty() && !p.eof && !p.failed)
            p.failed = !read_packet(p);
        if (p.failed)
            break;
        AVPacket *packet = nullptr;
        if (!p.video_packets.empty()) {
            packet = p.video_packets.front();
            p.video_packets.pop_front();
        } else if (p.flushed) {
            p.failed = !video_error("video ended before its frame count");
            break;
        } else {
            p.flushed = true;
        }
        rc = avcodec_send_packet(p.video, packet);
        av_packet_free(&packet);
        if (rc < 0) {
            p.failed = !decoder_error("send video packet", rc);
            break;
        }
    }
    p.video_ms += host_millis() - decode_start;
    return false;
}

void BinkDoFrame(X86 *c) {
    set_eax(c, 0);
    uint32_t rec = arg(c, 0);
    BinkPlayer *p = player_for(rec);
    if (!p || p->failed)
        return;
    // Some guests only react to keys/clicks inside the video loop. Finish
    // its record so the outer message loop can handle the pending close.
    if (host_close_requested()) {
        p->displayed = p->count;
        write_frame_count(rec, *p);
        return;
    }
    if (decode_frame(*p, rec))
        return;
    // Let a guest leave its playback loop after a fatal decoder error.
    p->displayed = p->count;
    write_frame_count(rec, *p);
}

void BinkNextFrame(X86 *c) {
    uint32_t rec = arg(c, 0);
    if (BinkPlayer *p = player_for(rec)) {
        if (host_close_requested()) {
            p->current = p->count;
            p->displayed = p->count;
        } else if (p->current < UINT32_MAX) {
            p->displayed = p->current;
            ++p->current;
        }
        write_frame_count(rec, *p);
        service_audio(rec, *p);
    }
    set_eax(c, 0);
}

// The SDK stores eight {x, y, width, height} rectangles at +0x34 and their
// count at +0xb4. A decoded frame dirties the whole image; before decode,
// clear the count so the guest never blits an uninitialised frame.
void BinkGetRects(X86 *c) {
    set_eax(c, 0);
    uint32_t rec = arg(c, 0);
    BinkPlayer *p = player_for(rec);
    if (!p)
        return;
    uint32_t count = p->have_frame ? 1 : 0;
    if (count) {
        wr32(rec + 0x34, 0);
        wr32(rec + 0x38, 0);
        wr32(rec + 0x3c, (uint32_t)p->frame->width);
        wr32(rec + 0x40, (uint32_t)p->frame->height);
    }
    wr32(rec + 0xb4, count);
    set_eax(c, count);
}

// BinkGetSummary fills the SDK's BINKSUMMARY. The real binkw32.dll's
// _BinkGetSummary@8 writes Width at +0, Height at +4, FrameRate at +0xc,
// FrameRateDiv at +0x10 and an HBINK+8 field at +0x20; runblack.exe's video
// player reads exactly those. Width/height/frame-rate/count come from the
// host decoder. The real call zeroes 0x7c bytes before filling them.
void BinkGetSummary(X86 *c) {
    set_eax(c, 0);
    uint32_t rec = arg(c, 0), summary = arg(c, 1);
    BinkPlayer *p = player_for(rec);
    if (!p || !summary || !gm_valid(summary, BINK_SUMMARY_BYTES))
        return;
    memset(g_mem + summary, 0, BINK_SUMMARY_BYTES);
    wr32(summary + 0x00, (uint32_t)p->video->width);
    wr32(summary + 0x04, (uint32_t)p->video->height);
    wr32(summary + 0x0c, (uint32_t)p->fps.num);
    wr32(summary + 0x10, (uint32_t)p->fps.den);
    wr32(summary + 0x20, p->count);
}

// _BinkGetRealtime@12(HBINK, BINKREALTIME*, u32 frames). The shipped Bink 1.0
// DLL (binkw32.dll 0x30006380) writes a 0x38-byte record: FrameNum, FrameRate,
// FrameRateDiv, Frames (the averaging window), FramesTime, VideoTime,
// AudioTime, ReadForegroundTime, ReadIdleTime, ReadBackgroundTime, BlitTime,
// BufferSize, BufferUsed and DataRate. The guest reads that whole layout after
// this call to format a playback debug line (see docs/shims.md).
//
// FrameNum, FrameRate/FrameRateDiv and the window size come from the player's
// real state. FramesTime is the pause-adjusted host clock; the stage times are
// this shim's own measurements of demux, decode and blit work, so the
// percentages stay consistent with real playback. Bink's per-frame history
// arrays do not exist here, so ReadBackgroundTime is 0 and BufferUsed/DataRate
// come from the compressed bytes actually read. SHIM(temporary).
void BinkGetRealtime(X86 *c) {
    set_eax(c, 0);
    uint32_t rec = arg(c, 0), out = arg(c, 1), frames_arg = arg(c, 2);
    BinkPlayer *p = player_for(rec);
    if (!p || !out || !gm_valid(out, BINK_REALTIME_BYTES))
        return;
    uint64_t elapsed = p->elapsed_millis();
    // Mirror the DLL's window clamp: a zero request means the whole file, and
    // the window never exceeds the frame count.
    uint32_t frames = frames_arg ? frames_arg : p->count;
    if (p->count && frames > p->count)
        frames = p->count;
    if (!frames)
        frames = 1;
    uint64_t busy = p->video_ms + p->audio_ms + p->read_ms + p->blit_ms;
    // The total is the greater of wall time and measured work so rounding a
    // sub-millisecond call cannot make a stage percentage exceed 100.
    uint64_t total = std::max<uint64_t>(elapsed, busy);
    if (!total)
        total = 1;
    uint64_t idle = total - busy;
    uint64_t rate = elapsed ? p->bytes_read * 1000 / elapsed : 0;
    uint64_t used = std::min<uint64_t>(p->bytes_read, p->total_bytes);

    wr32(out + 0x00, p->current);
    wr32(out + 0x04, (uint32_t)p->fps.num);
    wr32(out + 0x08, (uint32_t)p->fps.den);
    wr32(out + 0x0c, frames);
    wr32(out + 0x10, (uint32_t)total);
    wr32(out + 0x14, (uint32_t)p->video_ms);
    wr32(out + 0x18, (uint32_t)p->audio_ms);
    wr32(out + 0x1c, (uint32_t)p->read_ms);
    wr32(out + 0x20, (uint32_t)idle);
    wr32(out + 0x24, 0); // a single-threaded shim has no background reader
    wr32(out + 0x28, (uint32_t)p->blit_ms);
    wr32(out + 0x2c, (uint32_t)std::min<uint64_t>(p->total_bytes, UINT32_MAX));
    wr32(out + 0x30, (uint32_t)used);
    wr32(out + 0x34, (uint32_t)std::min<uint64_t>(rate, UINT32_MAX));
    log_once("bink.getrealtime",
             "SHIM(temporary): _BinkGetRealtime@12 reports host-clock playback "
             "accounting; stage times are measured by this shim, not Bink's own "
             "per-frame accumulators");
}

// BinkGoto seeks to a frame (flags 0) so a guest can show one logo frame and
// then another from the same file. The custom I/O seek callback carries the
// host-file window; the decoder and queued packets are flushed first.
void BinkGoto(X86 *c) {
    set_eax(c, 0);
    uint32_t rec = arg(c, 0);
    uint32_t frame = arg(c, 1);
    uint32_t flags = arg(c, 2);
    BinkPlayer *p = player_for(rec);
    if (!p)
        return;
    // binkw32's flags are a bitmask: flags & 3 == 3 repositions audio only and
    // leaves the video frame alone. RT3 uses 0 and 1, both video seeks.
    if ((flags & 3) == 3)
        return;
    if (frame < 1)
        frame = 1;
    if (frame > p->count)
        frame = p->count;
    for (AVPacket *packet : p->video_packets)
        av_packet_free(&packet);
    p->video_packets.clear();
    p->pending.clear();
    p->pending_pos = 0;
    p->eof = false;
    p->flushed = false;
    p->failed = false;
    p->have_frame = false;
    avcodec_flush_buffers(p->video);
    if (p->audio)
        avcodec_flush_buffers(p->audio);
    AVStream *stream = p->input->streams[p->video_index];
    int64_t ts = av_rescale_q((int64_t)(frame - 1), av_inv_q(p->fps), stream->time_base);
    int rc = av_seek_frame(p->input, p->video_index, ts, AVSEEK_FLAG_BACKWARD);
    if (rc < 0) {
        p->failed = !decoder_error("seek video frame", rc);
        p->displayed = p->count;
    } else {
        p->displayed = p->current;
        p->current = frame;
    }
    write_frame_count(rec, *p);
}

// Shift the playback origin by the time spent paused, preserving the time
// remaining until the next frame. Repeated pause/resume calls are harmless;
// unsigned subtraction also handles the host's millisecond counter wrapping.
void BinkPause(X86 *c) {
    if (BinkPlayer *p = player_for(arg(c, 0))) {
        if (arg(c, 1)) {
            if (!p->paused) {
                p->paused_at = host_millis();
                p->paused = true;
            }
        } else if (p->paused) {
            p->t0 += host_millis() - p->paused_at;
            p->paused = false;
        }
    }
    set_eax(c, 0);
}

// The guest copies frame N after NextFrame changes the counter to N+1.
// The wait ends at that counter's boundary using host time, never a guest
// rendering clock; unsigned subtraction also handles host_millis wrapping.
void BinkWait(X86 *c) {
    uint32_t rec = arg(c, 0);
    BinkPlayer *p = player_for(rec);
    if (p)
        service_audio(rec, *p);
    uint32_t wait = 0;
    if (p && !p->failed && !host_close_requested()) {
        // The ABI's millisecond expression truncates, rather than rounding.
        int64_t due =
            av_rescale_rnd((int64_t)(p->current - 1) * 1000, p->fps.den, p->fps.num, AV_ROUND_DOWN);
        wait = p->paused || (uint32_t)(host_millis() - p->t0) < (uint64_t)due;
    }
    set_eax(c, wait);
}

void BinkService(X86 *c) {
    set_eax(c, 0);
    uint32_t rec = arg(c, 0);
    if (BinkPlayer *p = player_for(rec))
        service_audio(rec, *p);
}

// Validate the entire destination rectangle before writing any row, using
// wide arithmetic so a guest offset or pitch cannot wrap into valid memory.
void BinkCopyToBuffer(X86 *c) {
    set_eax(c, 0);
    BinkPlayer *p = player_for(arg(c, 0));
    if (!p || !p->have_frame || p->failed)
        return;
    uint32_t flags = arg(c, 6);
    uint32_t type = flags & 0xff;
    uint32_t scaling = flags & 0x70000000;
    if (scaling && scaling != 0x30000000) {
        video_error("unsupported BinkCopyToBuffer scaling mode");
        return;
    }
    uint32_t xscale = scaling == 0x30000000 ? 2 : 1;
    if (type != VIDEO_RGB565 && type != VIDEO_RGB555 && type != VIDEO_XRGB8888)
        return;
    uint64_t bpp = type == VIDEO_XRGB8888 ? 4 : 2;
    uint64_t pitch = arg(c, 2), xbytes = (uint64_t)arg(c, 4) * bpp;
    uint64_t rows = std::min(arg(c, 3), (uint32_t)p->frame->height);
    uint64_t row_bytes = (uint64_t)p->frame->width * bpp * xscale;
    if (!rows)
        return;
    uint64_t start = (uint64_t)arg(c, 1) + (uint64_t)arg(c, 5) * pitch + xbytes;
    uint64_t span = (rows - 1) * pitch + row_bytes;
    if (!arg(c, 1) || xbytes + row_bytes > pitch || start > UINT32_MAX || span > UINT32_MAX ||
        !gm_valid((uint32_t)start, (uint32_t)span)) {
        video_error("copy destination is outside guest memory or pitch");
        return;
    }
    uint32_t blit_start = host_millis();
    for (uint32_t row = 0; row < rows; ++row) {
        uint8_t *dest = g_mem + start + row * pitch;
        video_frame_convert_row(dest, p->frame->data[0] + (ptrdiff_t)row * p->frame->linesize[0],
                                p->frame->data[1] + (ptrdiff_t)(row / 2) * p->frame->linesize[1],
                                p->frame->data[2] + (ptrdiff_t)(row / 2) * p->frame->linesize[2],
                                (uint32_t)p->frame->width, (VideoSurfaceType)type);
        if (xscale == 2)
            for (uint32_t x = (uint32_t)p->frame->width; x-- > 0;) {
                memmove(dest + x * 2 * bpp, dest + x * bpp, bpp);
                memcpy(dest + (x * 2 + 1) * bpp, dest + x * 2 * bpp, bpp);
            }
    }
    p->blit_ms += host_millis() - blit_start;
}

void BinkClose(X86 *c) {
    uint32_t rec = arg(c, 0);
    if (g_players.erase(rec))
        heap_free(rec);
    set_eax(c, 0);
}

// _BinkSetVolume@12(HBINK, U32 trackid, U32 volume): binkw32 forwards the
// value to the track's sound-system callback (binkw32.dll 0x30015900). The
// guest passes track 0 and a linear 0..65535 level from the movie volume
// setting; host-rendered Bink audio applies it directly.
int32_t bink_volume_mb(uint32_t volume) {
    if (volume == 0)
        return -10000;
    int32_t mb = (int32_t)(2000.0 * log10((double)volume / 65535.0));
    return mb > 0 ? 0 : (mb < -10000 ? -10000 : mb);
}

void BinkSetVolume(X86 *c) {
    uint32_t rec = arg(c, 0), track = arg(c, 1), volume = arg(c, 2);
    if (BinkPlayer *p = player_for(rec)) {
        if (track == 0) {
            p->volume_mb = bink_volume_mb(volume);
            if (p->channel >= 0)
                host_audio_set_volume(p->channel, p->volume_mb);
        }
    }
    set_eax(c, 0);
}

// _BinkSetSoundOnOff@8(HBINK, int). The guest calls it around the pre-intro to
// mute/unmute the movie's audio stream. The real SDK silences the Bink audio
// track; here the decoder's audio is host-rendered (a deferred fidelity gap),
// so the flag only gates service_audio and an active voice is stopped when the
// guest mutes. SHIM(temporary): no Bink audio is mixed into the guest mixer.
void BinkSetSoundOnOff(X86 *c) {
    uint32_t rec = arg(c, 0), on = arg(c, 1);
    if (BinkPlayer *p = player_for(rec)) {
        p->sound_on = on != 0;
        if (!p->sound_on && p->audio_started && p->channel >= 0) {
            host_audio_stop(p->channel);
            dx_free_audio_channel(p->channel);
            p->channel = -1;
            p->audio_started = false;
        }
    }
    log_once("bink.soundonoff",
             "SHIM(temporary): _BinkSetSoundOnOff@8 stores the flag and gates the host "
             "audio service; Bink audio is not mixed into the guest mixer");
    set_eax(c, 0);
}
#else
// A finished record makes a guest continue past cinematics on builds without
// FFmpeg. It is a successful skip, so GetError remains an empty string.
void BinkOpen(X86 *c) {
    set_eax(c, 0);
    g_error[0] = 0;
    uint32_t name = arg(c, 0), flags = arg(c, 1);
    if (!check_open_flags(flags))
        return;
    take_io_size(flags);
    if ((flags & BINK_FILE_HANDLE) && !win32_file_handle_position(name, nullptr, nullptr)) {
        video_error("invalid video file handle");
        return;
    }
    uint32_t rec = heap_alloc(BINK_RECORD_BYTES, true, 16);
    if (rec) {
        memset(g_mem + rec, 0, BINK_RECORD_BYTES);
        wr32(rec, 640);
        wr32(rec + 4, 480);
        if (flags & BINK_FILE_HANDLE)
            LOGV("bink: open handle %08x -> finished video record %08x (no decoder)", name, rec);
        else
            LOGV("bink: open \"%s\" -> finished video record %08x (no decoder)",
                 gm_str(name).c_str(), rec);
    } else {
        video_error("cannot allocate video record");
    }
    set_eax(c, rec);
}
void BinkClose(X86 *c) {
    if (arg(c, 0))
        heap_free(arg(c, 0));
    set_eax(c, 0);
}
void BinkDoFrame(X86 *c) {
    ret0(c);
}
void BinkNextFrame(X86 *c) {
    ret0(c);
}
void BinkGetRects(X86 *c) {
    ret0(c);
}
void BinkGetSummary(X86 *c) {
    set_eax(c, 0);
    uint32_t rec = arg(c, 0), summary = arg(c, 1);
    if (!rec || !summary || !gm_valid(summary, BINK_SUMMARY_BYTES))
        return;
    memset(g_mem + summary, 0, BINK_SUMMARY_BYTES);
    wr32(summary + 0x00, rd32(rec));
    wr32(summary + 0x04, rd32(rec + 4));
    wr32(summary + 0x0c, 1);
    wr32(summary + 0x10, 1);
}
// The no-decoder record is already finished: report a zeroed realtime record
// with a positive time base so the guest's percentage divisions stay defined.
void BinkGetRealtime(X86 *c) {
    set_eax(c, 0);
    uint32_t rec = arg(c, 0), out = arg(c, 1);
    if (!rec || !out || !gm_valid(out, BINK_REALTIME_BYTES))
        return;
    memset(g_mem + out, 0, BINK_REALTIME_BYTES);
    wr32(out + 0x00, rd32(rec + 0x14));
    wr32(out + 0x04, 1);
    wr32(out + 0x08, 1);
    wr32(out + 0x0c, 1);
    wr32(out + 0x10, 1);
    log_once("bink.getrealtime",
             "SHIM(temporary): _BinkGetRealtime@12 reports a finished record on a "
             "build without video decoding");
}
void BinkGoto(X86 *c) {
    // The no-decoder record is already finished; a seek is a no-op.
    ret0(c);
}
void BinkPause(X86 *c) {
    ret0(c);
}
void BinkWait(X86 *c) {
    ret0(c);
}
void BinkService(X86 *c) {
    ret0(c);
}
void BinkCopyToBuffer(X86 *c) {
    ret0(c);
}
void BinkSetVolume(X86 *c) {
    ret0(c);
}
// No decoder to silence; the record is already finished.
void BinkSetSoundOnOff(X86 *c) {
    log_once("bink.soundonoff",
             "SHIM(temporary): _BinkSetSoundOnOff@8 stores the flag and gates the host "
             "audio service; Bink audio is not mixed into the guest mixer");
    ret0(c);
}
#endif

// _BinkShouldSkip@4(HBINK): nonzero when the caller should drop the current
// frame because playback is behind. This shim decodes each frame synchronously
// and BinkWait blocks to the frame boundary, so no frame is ever late.
void BinkShouldSkip(X86 *c) {
    ret0(c);
}

void BinkDDSurfaceType(X86 *c) {
    ComObj *s = com_this_arg(c);
    uint32_t type = 0;
    if (s && s->kind == K_SURFACE) {
        if (s->bpp == 32 && s->rmask == 0xff0000)
            type = VIDEO_XRGB8888;
        else if (s->bpp == 16 && s->rmask == 0xf800)
            type = VIDEO_RGB565;
        else if (s->bpp == 16 && s->rmask == 0x7c00)
            type = VIDEO_RGB555;
    }
    set_eax(c, type);
}

// _BinkGetKeyFrame@12(HBINK, U32 frame, S32 flags): a pure query over
// binkw32's precomputed keyframe table (0x30014720). The only RT3 caller
// discards the result and immediately BinkGotos, and the host decoder has no
// equivalent table, so report 0 rather than inventing one.
void BinkGetKeyFrame(X86 *c) {
    ret0(c);
}

void BinkGetError(X86 *c) {
    if (!g_error_string)
        g_error_string = heap_alloc(sizeof g_error, true, 16);
    if (g_error_string)
        memcpy(g_mem + g_error_string, g_error, sizeof g_error);
    set_eax(c, g_error_string);
}

// _BinkSetIOSize@4(U32): binkw32 stores the value (0x30012070 writes it to
// 0x30065238); the next _BinkOpen@8 with flag 0x1000000 consumes it once.
void BinkSetIOSize(X86 *c) {
    g_io_size = arg(c, 0);
    LOGV("bink: I/O size %u", g_io_size);
}

#define BINK(name, bytes, fn) {"binkw32.dll", "_Bink" #name "@" #bytes, (bytes) / 4, fn}
#define SMACK(name, bytes, fn) {"smackw32.dll", "_Smack" #name "@" #bytes, (bytes) / 4, fn}

const ImportShim g_video_shims[] = {
    BINK(OpenDirectSound, 4, ret1),
    BINK(GetRects, 8, BinkGetRects),
    BINK(GetSummary, 8, BinkGetSummary),
    BINK(GetRealtime, 12, BinkGetRealtime),
    BINK(Goto, 12, BinkGoto),
    BINK(Pause, 8, BinkPause),
    BINK(Open, 8, BinkOpen),
    BINK(OpenMiles, 4, ret0),
    BINK(SetSoundSystem, 8, ret1),
    BINK(SetSoundOnOff, 8, BinkSetSoundOnOff),
    BINK(SetIOSize, 4, BinkSetIOSize),
    BINK(SetVolume, 12, BinkSetVolume),
    BINK(GetKeyFrame, 12, BinkGetKeyFrame),
    BINK(DDSurfaceType, 4, BinkDDSurfaceType),
    BINK(DoFrame, 4, BinkDoFrame),
    BINK(NextFrame, 4, BinkNextFrame),
    BINK(Wait, 4, BinkWait),
    BINK(CopyToBuffer, 28, BinkCopyToBuffer),
    BINK(ShouldSkip, 4, BinkShouldSkip),
    BINK(Service, 4, BinkService),
    BINK(GetError, 0, BinkGetError),
    BINK(Close, 4, BinkClose),
    BINK(BufferClose, 4, ret0),
    SMACK(Open, 12, ret0),
    SMACK(SoundUseMSS, 4, ret0),
    SMACK(DoFrame, 4, ret0),
    SMACK(NextFrame, 4, ret0),
    SMACK(Wait, 4, ret0),
    SMACK(ToBuffer, 28, ret0),
    SMACK(Close, 4, ret0),
};

} // namespace

void bink_shutdown() {
#ifdef RECOMP_HAVE_FFMPEG
    // Unlike reset, shutdown runs before the guest heap is discarded. Destroy
    // players here so their audio stops before the host's static state dies.
    while (!g_players.empty()) {
        auto it = g_players.begin();
        uint32_t rec = it->first;
        g_players.erase(it);
        heap_free(rec);
    }
#endif
}

void bink_reset() {
#ifdef RECOMP_HAVE_FFMPEG
    g_players.clear();
#endif
    // mem_init already discarded the old guest heap.
    g_io_size = 0;
    g_error_string = 0;
    g_error[0] = 0;
}

// A standalone player program the game runs (binkplay.exe): the movie plays
// in the game window, scaled to fit, until it ends or a key or click skips it.
extern "C" bool bink_play_movie(const char *guest_path) {
#ifdef RECOMP_HAVE_FFMPEG
    std::string path = win32_host_path_op(guest_path, WIN32_FILE_READ);
    if (path.empty())
        return video_error("cannot resolve movie filename");
    BinkPlayer p;
    if (!open_file_window(p, path, 0, 0) || !prepare_streams(p))
        return false;
    const int w = p.video->width, h = p.video->height;
    LOGW("bink: playing \"%s\" (%dx%d, %u frames)", guest_path, w, h, p.count);
    static const int skip_keys[] = {0x01, 0x02, 0x0d, 0x1b, 0x20};
    auto skipped = [] {
        return host_close_requested() ||
               std::any_of(std::begin(skip_keys), std::end(skip_keys), host_key_down);
    };
    std::vector<uint32_t> argb((size_t)w * h);
    p.t0 = host_millis();
    uint32_t n = 0;
    for (; n < p.count && !skipped() && decode_frame(p, 0); ++n) {
        for (int row = 0; row < h; ++row) {
            uint32_t *dest = argb.data() + (size_t)row * w;
            video_frame_convert_row(reinterpret_cast<uint8_t *>(dest),
                                    p.frame->data[0] + (ptrdiff_t)row * p.frame->linesize[0],
                                    p.frame->data[1] + (ptrdiff_t)(row / 2) * p.frame->linesize[1],
                                    p.frame->data[2] + (ptrdiff_t)(row / 2) * p.frame->linesize[2],
                                    (uint32_t)w, VIDEO_XRGB8888);
            for (int x = 0; x < w; ++x)
                dest[x] |= 0xff000000u;
        }
        host_display_present_window(argb.data(), w, h);
        int64_t due = av_rescale((int64_t)(n + 1) * 1000, p.fps.den, p.fps.num);
        for (int64_t left; !skipped() && (left = due - (int64_t)(host_millis() - p.t0)) > 0;) {
            guest_sleep_ms((uint32_t)std::min<int64_t>(left, 5));
            service_audio(0, p);
        }
    }
    LOGW("bink: \"%s\" %s at frame %u of %u", guest_path,
         p.failed      ? "failed"
         : n < p.count ? "skipped"
                       : "finished",
         n, p.count);
    while (!skipped() && p.audio_started && host_audio_queued_bytes(p.channel) > 0)
        guest_sleep_ms(5);
    while (std::any_of(std::begin(skip_keys), std::end(skip_keys), host_key_down))
        guest_sleep_ms(5);
    return true;
#else
    (void)guest_path;
    return false;
#endif
}

void bink_register() {
    static bool done = false;
    if (done)
        return;
    done = true;
    imports_register(g_video_shims, std::size(g_video_shims));
}
