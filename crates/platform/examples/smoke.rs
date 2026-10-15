//! Records the primary monitor for a few seconds with the hardware encoder and writes an MKV.
//! `cargo run -p fluxcapture-platform --example smoke -- out.mkv [seconds] [hevc]`

use fluxcapture_mux::codec::{annexb_to_length_prefixed, avcc_from_annexb, hvcc_from_annexb};
use fluxcapture_mux::mkv::{MkvOptions, MkvWriter};
use fluxcapture_mux::{ColorInfo, TrackSpec, VideoCodec, VideoTrack};
use fluxcapture_platform::video::{VideoEngine, VideoEngineConfig};
use fluxcapture_platform::{
    CaptureOptions, CaptureTarget, Crop, Fit, Layer, LayerSource, PreviewConfig, RateControl, RectF, ScaleFilter, VideoCodecKind, VideoEncoderConfig,
};
use fluxcapture_time::{FrameRate, Hns};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let out = args.get(1).cloned().unwrap_or_else(|| "smoke.mkv".into());
    let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    let hevc = args.get(3).is_some_and(|s| s == "hevc");
    let (w, h, fps) = (1920, 1080, FrameRate::FPS_60);
    let engine = VideoEngine::new(VideoEngineConfig {
        gpu: None,
        width: w,
        height: h,
        fps,
        scale_filter: ScaleFilter::Bicubic,
        preview: PreviewConfig { enabled: false, ..Default::default() },
        variable_frame_rate: false,
    })?;
    println!("GPU: {}", engine.gpu_name());
    engine.set_scene(vec![Layer {
        id: 1,
        source: LayerSource::Capture { target: CaptureTarget::Monitor { id: String::new() }, options: CaptureOptions::default() },
        rect: RectF { x: 0.0, y: 0.0, w: w as f32, h: h as f32 },
        fit: Fit::Contain,
        crop: Crop::default(),
        opacity: 1.0,
        visible: true,
    }]);
    let (tx, rx) = crossbeam_channel::bounded(600);
    let codec = if hevc { VideoCodecKind::Hevc } else { VideoCodecKind::H264 };
    let info = engine.start_encoder(
        VideoEncoderConfig {
            encoder_id: None,
            codec,
            width: w,
            height: h,
            fps,
            rate: RateControl::Cbr { kbps: 12_000 },
            keyframe_interval_s: 2.0,
            speed_quality: 50,
            low_latency: true,
            full_range: false,
        },
        tx,
    )?;
    println!("encoder: {} (hardware: {})", info.name, info.hardware);
    std::thread::sleep(std::time::Duration::from_millis(500));
    println!("sources: {:?}", engine.source_states());
    if let Ok(shot) = engine.screenshot() {
        let mut ppm = format!(
            "P6 {} {} 255
",
            shot.width, shot.height
        )
        .into_bytes();
        for px in shot.data.as_chunks::<4>().0 {
            ppm.extend_from_slice(&px[..3]);
        }
        std::fs::write(format!("{out}.ppm"), ppm)?;
        let sum: u64 = shot.data.iter().map(|&b| u64::from(b)).sum();
        println!("screenshot mean byte: {}", sum / shot.data.len() as u64);
    }
    let t0 = std::time::Instant::now();
    let mut packets = Vec::new();
    while t0.elapsed().as_secs() < secs {
        if let Ok(p) = rx.recv_timeout(std::time::Duration::from_millis(100)) {
            packets.push(p);
        }
    }
    let stats = engine.stats();
    engine.stop_encoder();
    packets.extend(rx.try_iter());
    println!("{} packets, stats: {stats:#?}", packets.len());
    let first_key = packets.iter().position(|p| p.keyframe).ok_or("no keyframe")?;
    let mc = if hevc { VideoCodec::Hevc } else { VideoCodec::H264 };
    let cfg = if hevc { hvcc_from_annexb(&packets[first_key].data)? } else { avcc_from_annexb(&packets[first_key].data)? };
    let track = TrackSpec::Video(VideoTrack {
        codec: mc,
        width: w,
        height: h,
        frame_rate: Some(fps),
        codec_private: cfg,
        color: ColorInfo { full_range: false },
        name: "Video".into(),
    });
    let file = std::io::BufWriter::new(std::fs::File::create(&out)?);
    let mut mkv = MkvWriter::new(file, &[track], MkvOptions::default())?;
    let base = packets[first_key].pts;
    let mut buf = Vec::new();
    for p in &packets[first_key..] {
        annexb_to_length_prefixed(mc, &p.data, &mut buf)?;
        mkv.write(0, Hns(p.pts.0 - base.0), p.keyframe, &buf)?;
    }
    mkv.finish()?;
    println!("wrote {out}");
    Ok(())
}
