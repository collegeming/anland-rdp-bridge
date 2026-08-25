/* anland-rdp-bridge: PipeWire desktop-audio capture shim.
 *
 * Owns a virtual `Audio/Sink` node ("anland-rdp-speaker"); desktop apps mix
 * their playback into it and the `process` callback buffers the S16LE PCM
 * for the Rust audio pump to drain via `anland_rdp_audio_pull`. The rate is
 * fixed at the RDPSND-advertised 44100 Hz / stereo so the bytes ship over
 * RDPSND unchanged (PipeWire resamples from the hardware rate internally).
 *
 * Pattern adapted from anland's `anland_audio.c` (MIT OR Apache-2.0): the
 * same virtual-sink + thread-loop + auto-reconnect structure, minus the
 * socket/mic half that the Android consumer needed.
 *
 * Licensed under MIT OR Apache-2.0 (project dual license). */

#define _GNU_SOURCE
#include "anland_rdp_audio.h"

#include <errno.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <pipewire/pipewire.h>
#include <spa/param/audio/format-utils.h>
#include <spa/pod/builder.h>
#include <spa/utils/hook.h>

/* Fixed to match the RDPSND-advertised PCM format so waves ship unchanged. */
#define CAP_RATE     44100u
#define CAP_CHANNELS 2u
/* ~1s of stereo S16 audio; bounds latency, oldest bytes drop on overflow. */
#define MAX_BUFFER_BYTES (CAP_RATE * CAP_CHANNELS * (int)sizeof(int16_t))
#define RECONNECT_SECS 1

struct state {
    struct pw_thread_loop *loop;
    struct pw_context     *context;
    struct pw_core        *core;
    struct spa_hook        core_listener;
    struct spa_source     *reconnect_timer;
    bool                   pw_connected;

    struct pw_stream      *capture;   /* virtual Audio/Sink */
    struct spa_hook        capture_listener;

    /* Capture ring. Written from the PipeWire loop thread, read from the
     * Rust pump thread; mutex-guarded. Contiguous buffer with memmove drain
     * (audio is non-critical, sizes are small). */
    pthread_mutex_t        lock;
    uint8_t              *buf;
    size_t                buf_cap;
    size_t                buf_len;
};

static struct state *g = NULL;

static const struct spa_pod *build_format(struct spa_pod_builder *bld,
                                          uint32_t rate, uint32_t channels)
{
    struct spa_audio_info_raw info = {
        .format = SPA_AUDIO_FORMAT_S16_LE,
        .rate = rate,
        .channels = channels,
    };
    if (channels >= 2) {
        info.position[0] = SPA_AUDIO_CHANNEL_FL;
        info.position[1] = SPA_AUDIO_CHANNEL_FR;
    } else {
        info.position[0] = SPA_AUDIO_CHANNEL_MONO;
    }
    return spa_format_audio_raw_build(bld, SPA_PARAM_EnumFormat, &info);
}

static int connect_capture(struct pw_stream *stream, uint32_t rate, uint32_t channels)
{
    uint8_t buffer[1024];
    struct spa_pod_builder bld = SPA_POD_BUILDER_INIT(buffer, sizeof(buffer));
    const struct spa_pod *params[1] = { build_format(&bld, rate, channels) };
    return pw_stream_connect(stream, PW_DIRECTION_INPUT, PW_ID_ANY,
                             PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS,
                             params, 1);
}

/* Mixed desktop PCM arrives here on the loop thread -> append to the ring,
 * dropping oldest bytes if the RDP pump has fallen behind. */
static void on_capture_process(void *data)
{
    struct state *s = data;
    struct pw_buffer *b = pw_stream_dequeue_buffer(s->capture);
    if (!b)
        return;
    struct spa_data *d = &b->buffer->datas[0];
    if (d->data && d->chunk->size > 0) {
        size_t n = d->chunk->size;
        const uint8_t *p = (const uint8_t *)d->data + d->chunk->offset;
        pthread_mutex_lock(&s->lock);
        if (s->buf_len + n > s->buf_cap) {
            size_t drop = (s->buf_len + n) - s->buf_cap;
            if (drop >= s->buf_len) {
                s->buf_len = 0;
            } else {
                memmove(s->buf, s->buf + drop, s->buf_len - drop);
                s->buf_len -= drop;
            }
        }
        if (n > s->buf_cap - s->buf_len)
            n = s->buf_cap - s->buf_len;   /* keep the newest if a single period overflows */
        memcpy(s->buf + s->buf_len, p, n);
        s->buf_len += n;
        pthread_mutex_unlock(&s->lock);
    }
    pw_stream_queue_buffer(s->capture, b);
}

static const struct pw_stream_events capture_events = {
    PW_VERSION_STREAM_EVENTS,
    .process = on_capture_process,
};

static void arm_reconnect(struct state *s)
{
    struct timespec val = { .tv_sec = RECONNECT_SECS, .tv_nsec = 0 };
    pw_loop_update_timer(pw_thread_loop_get_loop(s->loop), s->reconnect_timer,
                         &val, NULL, false);
}

/* Sound service (pipewire/wireplumber) restarted: drop the dead core/streams
 * and let the timer rebuild. The capture ring is untouched so the pump resumes
 * seamlessly once PipeWire is back. */
static void on_core_error(void *data, uint32_t id, int seq, int res, const char *message)
{
    struct state *s = data;
    (void)seq; (void)message;
    if (id == PW_ID_CORE && res == -EPIPE) {
        s->pw_connected = false;
        arm_reconnect(s);
    }
}

static const struct pw_core_events core_events = {
    PW_VERSION_CORE_EVENTS,
    .error = on_core_error,
};

static void teardown_pw(struct state *s)
{
    if (s->capture) {
        spa_hook_remove(&s->capture_listener);
        pw_stream_destroy(s->capture);
        s->capture = NULL;
    }
    if (s->core) {
        spa_hook_remove(&s->core_listener);
        pw_core_disconnect(s->core);
        s->core = NULL;
    }
}

/* (Re)create the core + virtual sink. Returns 0 on success. The virtual sink
 * outranks the auto-null dummy (priority 1010) so WirePlumber makes it the
 * default and desktop audio routes here even with no other output device. */
static int build_pw(struct state *s)
{
    s->core = pw_context_connect(s->context, NULL, 0);
    if (!s->core)
        return -1;
    pw_core_add_listener(s->core, &s->core_listener, &core_events, s);

    s->capture = pw_stream_new(s->core, "anland-rdp-speaker",
        pw_properties_new(
            PW_KEY_MEDIA_TYPE, "Audio",
            PW_KEY_MEDIA_CLASS, "Audio/Sink",
            PW_KEY_NODE_NAME, "anland-rdp-speaker",
            PW_KEY_NODE_DESCRIPTION, "Anland RDP bridge speaker",
            PW_KEY_PRIORITY_SESSION, "1010",
            PW_KEY_PRIORITY_DRIVER, "1010",
            NULL));
    if (!s->capture)
        return -1;
    pw_stream_add_listener(s->capture, &s->capture_listener, &capture_events, s);

    if (connect_capture(s->capture, CAP_RATE, CAP_CHANNELS) < 0)
        return -1;
    return 0;
}

static void on_reconnect_timer(void *data, uint64_t expirations)
{
    struct state *s = data;
    (void)expirations;
    if (s->pw_connected)
        return;
    teardown_pw(s);
    if (build_pw(s) == 0)
        s->pw_connected = true;
    else {
        teardown_pw(s);
        arm_reconnect(s);
    }
}

int anland_rdp_audio_start(const char *target_name)
{
    (void)target_name;   /* reserved for routing; the virtual sink is self-contained */
    if (g)
        return 0;

    pw_init(NULL, NULL);

    struct state *s = calloc(1, sizeof(*s));
    if (!s)
        return -1;
    s->buf_cap = MAX_BUFFER_BYTES;
    pthread_mutex_init(&s->lock, NULL);
    s->buf = malloc(s->buf_cap);
    if (!s->buf)
        goto fail;

    s->loop = pw_thread_loop_new("anland-rdp-audio", NULL);
    if (!s->loop)
        goto fail;
    s->context = pw_context_new(pw_thread_loop_get_loop(s->loop), NULL, 0);
    if (!s->context)
        goto fail;
    s->reconnect_timer = pw_loop_add_timer(pw_thread_loop_get_loop(s->loop),
                                            on_reconnect_timer, s);
    if (!s->reconnect_timer)
        goto fail;
    if (pw_thread_loop_start(s->loop) < 0)
        goto fail;

    pw_thread_loop_lock(s->loop);
    if (build_pw(s) == 0)
        s->pw_connected = true;
    else {
        teardown_pw(s);
        arm_reconnect(s);
    }
    pw_thread_loop_unlock(s->loop);

    g = s;
    return 0;

fail:
    if (s->reconnect_timer)
        pw_loop_destroy_source(pw_thread_loop_get_loop(s->loop), s->reconnect_timer);
    if (s->context)
        pw_context_destroy(s->context);
    if (s->loop)
        pw_thread_loop_destroy(s->loop);
    free(s->buf);
    pthread_mutex_destroy(&s->lock);
    free(s);
    pw_deinit();
    return -1;
}

int anland_rdp_audio_pull(void *buf, uint32_t max_bytes,
                          uint32_t *rate, uint32_t *channels)
{
    if (!g)
        return -1;
    struct state *s = g;
    if (rate)
        *rate = CAP_RATE;
    if (channels)
        *channels = CAP_CHANNELS;
    if (max_bytes == 0)
        return 0;
    pthread_mutex_lock(&s->lock);
    uint32_t n = s->buf_len < max_bytes ? (uint32_t)s->buf_len : max_bytes;
    memcpy(buf, s->buf, n);
    if (n < s->buf_len)
        memmove(s->buf, s->buf + n, s->buf_len - n);
    s->buf_len -= n;
    pthread_mutex_unlock(&s->lock);
    return (int)n;
}

void anland_rdp_audio_stop(void)
{
    struct state *s = g;
    if (!s)
        return;
    g = NULL;
    if (s->loop)
        pw_thread_loop_stop(s->loop);
    teardown_pw(s);
    if (s->reconnect_timer)
        pw_loop_destroy_source(pw_thread_loop_get_loop(s->loop), s->reconnect_timer);
    if (s->context)
        pw_context_destroy(s->context);
    if (s->loop)
        pw_thread_loop_destroy(s->loop);
    free(s->buf);
    pthread_mutex_destroy(&s->lock);
    free(s);
    pw_deinit();
}
