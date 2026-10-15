# fluxcapture-audio

Layer L1. Depends only on `fluxcapture-time`, `serde`, `log`, `thiserror`. No `unsafe`, no panicking
shortcuts, no allocation in `Mixer::mix` once warmed up, no locks, no per-call logging.

```
WASAPI threads ──AudioChunk (f32, QPC time of first frame)──▶ Mixer::push
                                                            │  NaN/inf → 0, downmix/upmix to stereo
                                                            │  timeline placement + drift control
                                                            │  polyphase resampling to 48 kHz
                                                            ▼
                                                    per-source FIFO (2 s, bounded)
mixer thread, every ~10 ms ── Mixer::mix(start, frames) ──▶ │  filters: gain → high-pass → noise
                                                            │  suppression → gate → compressor →
                                                            │  mono / volume / balance
                                                            ▼
                                     tracks 1..=6: Σ routed sources → master volume → limiter
```

## The output timeline

The output is a fixed 48 kHz stereo timeline in the QPC domain. The caller computes each window's
`start` from a fixed base with `SampleClock` (`start_n = base + time_of(n)`), so consecutive windows
are contiguous; the mixer detects this (± half a sample) and otherwise treats the call as a
discontinuity (re-anchors its timeline at `start`, discards stale audio, primes its delay lines).

Internally every position is an *output frame index*. `Timeline` maps QPC time to a fractional
frame position (`anchor_pos + (t − anchor_time) · 48000 / 10⁷`).

## Placing a source on the timeline, and clock drift

Each source has

* `write_pos` — the frame position the resampler's next output frame will occupy; the FIFO holds
  the `len` frames just before it (`head = write_pos − len`);
* the resampler's exact knowledge of where the next *input* frame will land:
  `contiguous = write_pos + (buffered_input − pos) / step` (the windowed-sinc kernel is centred, so
  there is no group delay to account for).

For every chunk the **timestamp position** is `timeline(chunk.time + sync_offset)`.

1. **Anchoring.** The first chunk, a chunk flagged `discontinuity`, a sample-rate change, or an
   error beyond ±100 ms places the chunk's first frame *exactly* at its timestamp position (to a
   fraction of a sample: the fractional part becomes the resampler's initial phase). A forward jump
   leaves silence (the end of the previous segment is flushed out of the kernel first); a backward
   jump drops the overlapping newest audio. Counted in `SourceStats::discontinuities`.
2. **Contiguity.** Otherwise the chunk is appended right after the previous one, whatever its
   timestamp says. Timestamp jitter of a few ms therefore never reaches the audio.
3. **Drift control.** `error = timestamp position − contiguous position` is the clock error. A
   device whose clock runs `d` fast relative to QPC delivers `d·t` too much audio, so the error
   ramps as `de/dt = c − d`, where `c` is the ratio trim applied to the resampler
   (`step = nominal · (1 + c)`). A PI controller closes that loop:
   * error clamped to ±20 ms (outliers cannot wind it up), low-passed with τ = 1 s (jitter of ±3 ms
     per 10 ms packet averages to well under 0.1 ms);
   * `c = −(0.14·e + 0.01·∫e)` → natural frequency 0.1 rad/s, damping ≈ 0.7: settles in about a
     minute, zero steady-state error for a constant drift, residual pitch wobble of a few ppm;
   * `c` clamped to ±0.5 % (anti-windup on the integrator) — inaudible, and 25× the ±200 ppm a real
     device drifts.

   The integral term is the reported `drift_ppm`; it survives re-anchoring (a gap does not change
   the device's clock). Measured: +150 ppm with ±2 ms jitter for 10 simulated minutes keeps every
   marker within 0.1 ms of its true time, the estimate lands at 150 ± 3 ppm, no underruns/overruns.
4. **Loopback** (`continuous = false`) sends nothing while nothing plays. A forward error above
   20 ms re-anchors such a source instead of steering the controller, so a resume after a pause is
   silence followed by audio at exactly the right time — never a stretched catch-up. Counted in
   `SourceStats::gaps` (normal), and missing data is silence, not an underrun.
5. **Sync offset** (`-2000..=2000` ms, positive delays) is part of the timestamp position. Changing
   it live shifts the buffered audio immediately (a jump, then zero error).

At mix time, frames older than the window are discarded (`late_frames`), positions before the FIFO
head are silence, and positions after `write_pos` are missing — silence, plus an underrun episode
for continuous sources (`underruns`, `underrun_frames`). The FIFO is bounded to 2 s: overflow drops
the oldest audio and counts `overruns`.

## Resampler

Polyphase Kaiser-windowed sinc with a continuously variable ratio, streaming with state across
chunks, position kept in 32.32 fixed point.

| device rate | kernel | phases | quality (measured) |
|---|---|---|---|
| 48 kHz (ratio = 1 ± drift trim) | 24 taps, β = 7, cutoff 21.8 kHz | 4096, nearest (the phase only moves with the trim; the table is shared and stays in cache) | flat within 0.002 dB to 15 kHz; with a 500 ppm trim SNR 87 dB @ 1 kHz, 83 dB @ 8 kHz, 76 dB @ 15 kHz |
| other (44.1, 32, 96, 192 kHz…) | 32 taps × decimation factor, β = 8.6, cutoff 0.91 × lower Nyquist | 256, linearly interpolated | 44.1 k→48 k: 1 kHz SNR 94 dB, 12 kHz 94 dB; 96 k→48 k 30 kHz alias −90 dB |

Content between 24 and ~26 kHz in a high-rate source can fold only into the inaudible 22–24 kHz
band (transition band trade-off).

## Filters

* **Gain** −30..+30 dB, ramped.
* **High-pass**: 2nd-order Butterworth (RBJ biquad), 10–1000 Hz.
* **Noise suppression**: 1024-point STFT (21.3 ms), 50 % overlap, sqrt-Hann analysis + synthesis;
  both channels share one complex FFT and one real gain per bin (stereo image preserved). Noise floor
  by minimum statistics (minimum of the smoothed periodogram over ~1 s); Wiener gain with the
  decision-directed a-priori SNR (α = 0.98, keeps residual noise smooth, no musical noise), 3-bin
  frequency smoothing, gain floor −30 dB × strength.
* **Gate**: peak envelope (instant attack, 20 ms decay), open/close hysteresis, hold, linear
  attack/release fades; closed = silence.
* **Compressor**: feed-forward, stereo-linked RMS detector (5 ms), 6 dB soft knee, attack/release on
  the gain reduction, makeup gain.
* **Mono / volume / balance**: mono sum, fader (−60..+20 dB) and balance (centre = unity on both
  sides); every change, mute included, ramps over 10 ms.
* **Master** per track: volume (ramped) → lookahead peak limiter (5 ms) guaranteeing
  `|y| ≤ ceiling` for every sample: the gain is a boxcar average of a sliding minimum of the
  required gain (plus a one-pole release that can only lower it), which provably never exceeds the
  required gain of the sample it is applied to; a final clamp absorbs rounding.

NaN/inf samples become 0 (and magnitudes are bounded to ±64) before any filter sees them.

## Latencies — what the platform layer must know

| stage | latency | compensated? |
|---|---|---|
| noise suppression | 1024 frames (21.33 ms), constant, present even when disabled | yes, read-ahead |
| limiter | 240 frames (5 ms), constant, present even when disabled | yes, read-ahead |
| resampler | zero phase; needs ≤ 0.5 ms of input lookahead (≤ 2.7 ms at 192 kHz) | n/a |

The mixer reads source audio `PROCESSING_LOOKAHEAD_FRAMES` (1264 frames, 26.33 ms) ahead of the
window it outputs, so the output is **time-aligned**: sample `n` of the window starting at `start` is
the audio captured at `start + n / 48 kHz`. Therefore:

* **Do not** add encoder delay / priming offsets for the limiter or noise suppression. (The AAC
  encoder's own priming delay is still the encoder's business.)
* Run `mix` at least `processing_lookahead()` + device period + jitter behind real time;
  `RECOMMENDED_MIX_DELAY` = 80 ms is comfortable for 10 ms WASAPI periods. Too little shows up as
  underruns on continuous sources.
* Toggling noise suppression or the limiter never moves the timeline (both stages keep their delay
  line when disabled, and noise suppression hands its buffered audio over seamlessly).
* On the first window and after any discontinuous `start`, the mixer primes its delay lines over the
  lookahead span, so audio at the very start of a recording is not lost.

## Performance

`tests/realtime.rs`: 4 sources (48 kHz stereo, 44.1 kHz stereo, 48 kHz mono, 48 kHz 5.1) with
every filter, 2 tracks, limiter, 60 s of audio: ≈ 0.6–1.0 s in the test profile on a Ryzen 5 4600G
that was ~80 % busy with other work (≥ 60× realtime). A single 96 kHz 5.1 source with all filters:
≈ 0.2–0.35 s per minute.

## Limitations

* Channel layouts are inferred from the channel count (WAVE order); no channel mask.
* The limiter is sample-peak, not true-peak.
* Noise suppression targets stationary noise (fans, hiss, hum); it is not a voice-isolation model.
* `MixBuffers` tracks beyond `settings.tracks` are zero-filled, not removed.
