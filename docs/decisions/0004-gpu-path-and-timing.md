# 0004 — GPU-only recording path, scheduled timestamps, one clock

**Status:** accepted · **Plan reference:** §5.1–5.6

## Decisions

1. **One device, one GPU.** Capture, composition, colour conversion and encoding share one Direct3D 11
   device. "Automatic" picks the adapter that drives the primary monitor if it has a hardware encoder.
2. **Copy once.** A Windows.Graphics.Capture frame is copied into the source's own texture inside the
   frame callback and released at once (the OS buffer pool never stalls); the render thread samples the
   latest copy.
3. **Shader conversion to NV12.** Two passes write the Y and UV planes of NV12 surfaces directly
   (BT.709, TV or full range, MPEG-2 chroma siting with a [1 2 1] filter). Exact and independent of
   driver video-processor quirks.
4. **Scheduled timestamps.** Frame `k` is stamped `epoch + FrameRate::frame_time(k)`; pacing jitter
   affects which capture frame is sampled, never the file timing. A tick missed by whole frames is
   counted as a render-lag drop; the clock restarts after blocking set-up (encoder start), so start-up
   is not reported as loss.
5. **One clock.** All timestamps are 100 ns QPC ticks (`Hns`): WGC frame times, WASAPI QPC positions,
   the render clock and Media Foundation sample times. A/V sync is arithmetic, not heuristics.
6. **Overload policy.** First reduce non-essential work (the preview drops to 5 fps automatically and
   the user is told), then report the bottleneck with a proposal. The output resolution, frame rate or
   quality is never changed without the user.
