/* anland-rdp-bridge: PipeWire desktop-audio capture shim.
 *
 * Exposes a minimal C ABI to the Rust [`AnlandAudioSource`]. Mirrors the
 * proven virtual-sink pattern from anland's `anland_audio.c`: it owns a
 * `Audio/Sink` node ("anland-rdp-speaker") whose mixed-input PCM the RDP
 * server pulls and ships over RDPSND. No mic/source side and no socket I/O
 * — the C ring is drained directly from the Rust audio pump thread.
 *
 * Threading: the process callback runs on the PipeWire thread loop and writes
 * the ring under a mutex; `anland_rdp_audio_pull` runs from the Rust pump
 * thread and reads under the same mutex. Audio is non-critical so brief
 * contention is acceptable.
 *
 * Licensed under MIT OR Apache-2.0 (project dual license). */

#ifndef ANLAND_RDP_AUDIO_H
#define ANLAND_RDP_AUDIO_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Start the PipeWire virtual sink "anland-rdp-speaker" (and its capture path).
 * Returns 0 on success, -1 on failure. Idempotent: a no-op if already started. */
int anland_rdp_audio_start(void);

/* Pull captured S16LE interleaved PCM into `buf` (up to `max_bytes`).
 * On success returns the byte count written (0 when the ring is empty).
 * `*rate` and `*channels` receive the negotiated stream format (44100/2 by
 * default until PipeWire renegotiates). Returns -1 when not started. */
int anland_rdp_audio_pull(void *buf, uint32_t max_bytes,
                          uint32_t *rate, uint32_t *channels);

/* Stop and tear down the PipeWire connection. Idempotent. Safe to call from
 * a thread other than the one that started; blocks on the loop join. */
void anland_rdp_audio_stop(void);

#ifdef __cplusplus
}
#endif

#endif /* ANLAND_RDP_AUDIO_H */
