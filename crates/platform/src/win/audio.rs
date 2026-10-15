//! WASAPI capture (playback loopback, microphones, single processes) and the AAC encoder.
//!
//! Each capture runs on its own MMCSS "Pro Audio" thread, woken by the device event every ~10 ms,
//! and forwards chunks stamped with the device's QPC position (the same clock as video) to the
//! mixer. Shared mode with `AUTOCONVERTPCM` makes Windows deliver float32 / 48 kHz / stereo
//! directly; when a driver refuses that, the device's own mix format is converted here. A device
//! that disappears (unplugged headset, default device changed) is reopened automatically.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, TrySendError, bounded};
use fluxcapture_audio::{AudioChunk, AudioFormat, SourceId};
use fluxcapture_time::Hns;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{BLOB, CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{IUnknown, Interface, PCWSTR, implement};

use super::mf;
use super::{Context, ensure_com, to_wide};
use crate::clock::{ThreadKind, boost_thread};
use crate::{AudioCaptureState, AudioDeviceInfo, AudioEndpoint, AudioFlow, AudioProcessInfo, EncodedAudio, PlatformError};

fn enumerator() -> crate::Result<IMMDeviceEnumerator> {
    ensure_com()?;
    // SAFETY: creates the system device enumerator.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.ctx("cannot access audio devices")
}

fn device_id(d: &IMMDevice) -> String {
    // SAFETY: GetId returns a CoTaskMemAlloc'ed string, converted then freed.
    unsafe {
        let Ok(p) = d.GetId() else { return String::new() };
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn device_name(d: &IMMDevice) -> String {
    // SAFETY: reads the friendly-name property; the PROPVARIANT is cleared by its Drop.
    unsafe {
        d.OpenPropertyStore(STGM_READ)
            .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
            .map(|v| v.to_string())
            .unwrap_or_else(|_| "Audio device".into())
    }
}

/// Every active audio endpoint.
pub fn list_audio_devices() -> crate::Result<Vec<AudioDeviceInfo>> {
    let e = enumerator()?;
    let mut out = Vec::new();
    for (flow, kind) in [(eRender, AudioFlow::Playback), (eCapture, AudioFlow::Recording)] {
        // SAFETY: plain enumeration on a live enumerator.
        let default_id = unsafe { e.GetDefaultAudioEndpoint(flow, eConsole) }.map(|d| device_id(&d)).unwrap_or_default();
        // SAFETY: as above.
        let Ok(col) = (unsafe { e.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }) else { continue };
        // SAFETY: as above.
        let n = unsafe { col.GetCount() }.unwrap_or(0).min(128);
        for i in 0..n {
            // SAFETY: index below GetCount.
            let Ok(d) = (unsafe { col.Item(i) }) else { continue };
            let id = device_id(&d);
            out.push(AudioDeviceInfo { default: id == default_id, name: device_name(&d), id, flow: kind });
        }
    }
    Ok(out)
}

/// Processes with an audio session on the default playback device (candidates for per-app capture).
pub fn list_audio_processes() -> crate::Result<Vec<AudioProcessInfo>> {
    let e = enumerator()?;
    let mut out: Vec<AudioProcessInfo> = Vec::new();
    // SAFETY: documented COM calls on live objects; indices stay below GetCount.
    unsafe {
        let dev = e.GetDefaultAudioEndpoint(eRender, eConsole).ctx("no playback device")?;
        let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None).ctx("audio sessions")?;
        let sessions = mgr.GetSessionEnumerator().ctx("audio sessions")?;
        let n = sessions.GetCount().unwrap_or(0).clamp(0, 256);
        for i in 0..n {
            let Ok(s) = sessions.GetSession(i) else { continue };
            let Ok(s2) = s.cast::<IAudioSessionControl2>() else { continue };
            let Ok(pid) = s2.GetProcessId() else { continue };
            if pid == 0 || out.iter().any(|p| p.pid == pid) {
                continue;
            }
            let name = Some(super::display::process_name(pid)).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("PID {pid}"));
            out.push(AudioProcessInfo { pid, name });
        }
    }
    Ok(out)
}

/// Per-process capture needs Windows 10 build 20348 / Windows 11.
pub fn process_capture_supported() -> bool {
    super::system::os_version().2 >= 20348
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum SampleKind {
    F32,
    I16,
    I24,
    I32,
}

struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
    rate: u32,
    channels: u16,
    kind: SampleKind,
    device_id: String,
    name: String,
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: stops the client and closes the event this stream owns.
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

fn float_stereo_48k() -> WAVEFORMATEXTENSIBLE {
    let mut f = WAVEFORMATEXTENSIBLE::default();
    f.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE as u16;
    f.Format.nChannels = 2;
    f.Format.nSamplesPerSec = 48_000;
    f.Format.wBitsPerSample = 32;
    f.Format.nBlockAlign = 8;
    f.Format.nAvgBytesPerSec = 48_000 * 8;
    f.Format.cbSize = (std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>()) as u16;
    f.Samples.wValidBitsPerSample = 32;
    f.dwChannelMask = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT;
    f.SubFormat = KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    f
}

/// Sample layout of a WAVEFORMATEX(TENSIBLE).
///
/// # Safety
/// `f` must point at a valid WAVEFORMATEX whose `cbSize` extra bytes are readable.
unsafe fn parse_format(f: *const WAVEFORMATEX) -> crate::Result<(u32, u16, SampleKind)> {
    // SAFETY: caller contract.
    let base = unsafe { std::ptr::read_unaligned(f) };
    let tag = u32::from(base.wFormatTag);
    let sub = if tag == WAVE_FORMAT_EXTENSIBLE && usize::from(base.cbSize) >= 22 {
        // SAFETY: cbSize ≥ 22 means the extensible part is present.
        Some(unsafe { std::ptr::read_unaligned(f.cast::<WAVEFORMATEXTENSIBLE>()) }.SubFormat)
    } else {
        None
    };
    let float = tag == WAVE_FORMAT_IEEE_FLOAT || sub == Some(KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
    let pcm = tag == WAVE_FORMAT_PCM || sub == Some(KSDATAFORMAT_SUBTYPE_PCM);
    let kind = match (float, pcm, base.wBitsPerSample) {
        (true, _, 32) => SampleKind::F32,
        (_, true, 16) => SampleKind::I16,
        (_, true, 24) => SampleKind::I24,
        (_, true, 32) => SampleKind::I32,
        (_, _, bits) => return Err(PlatformError(format!("unsupported device format (tag {tag}, {bits} bits)"))),
    };
    if base.nSamplesPerSec == 0 || base.nChannels == 0 || base.nChannels > 32 {
        return Err(PlatformError("invalid device format".into()));
    }
    Ok((base.nSamplesPerSec, base.nChannels, kind))
}

/// Delivers the result of ActivateAudioInterfaceAsync.
#[implement(IActivateAudioInterfaceCompletionHandler, windows::Win32::System::Com::IAgileObject)]
struct Activated {
    tx: Sender<()>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let _ = self.tx.try_send(());
        Ok(())
    }
}

impl windows::Win32::System::Com::IAgileObject_Impl for Activated_Impl {}

fn process_client(pid: u32) -> crate::Result<IAudioClient> {
    if !process_capture_supported() {
        return Err(PlatformError("capturing a single app's audio needs Windows 11 (or Windows 10 build 20348+)".into()));
    }
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // The PROPVARIANT only borrows `params`: it must never be cleared (PropVariantClear would free
    // stack memory), hence ManuallyDrop.
    let mut pv = ManuallyDrop::new(PROPVARIANT::default());
    // SAFETY: writes the VT_BLOB arm of the union; `params` outlives every use of `pv`.
    unsafe {
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: (&params as *const AUDIOCLIENT_ACTIVATION_PARAMS).cast_mut().cast(),
        };
    }
    let (tx, rx) = bounded(1);
    let handler: IActivateAudioInterfaceCompletionHandler = Activated { tx }.into();
    // SAFETY: documented call; `pv` and `params` stay alive until the completion is awaited below.
    let op = unsafe { ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv), &handler) }
        .ctx("cannot capture this app's audio")?;
    rx.recv_timeout(Duration::from_secs(5)).map_err(|_| PlatformError("audio activation timed out".into()))?;
    let (mut hr, mut unk) = (windows::core::HRESULT(0), None::<IUnknown>);
    // SAFETY: the operation completed; live out-locals.
    unsafe { op.GetActivateResult(&mut hr, &mut unk) }.ctx("audio activation")?;
    if hr.is_err() {
        return Err(PlatformError(format!("cannot capture this app's audio (0x{:08X})", hr.0 as u32)));
    }
    unk.ok_or("audio activation returned nothing")?.cast().ctx("IAudioClient")
}

fn open(endpoint: &AudioEndpoint) -> crate::Result<Stream> {
    let (client, loopback, device_id, name) = match endpoint {
        AudioEndpoint::Process { pid, name } => (process_client(*pid)?, true, format!("process:{pid}"), name.clone()),
        _ => {
            let e = enumerator()?;
            // SAFETY: documented COM calls; the id string outlives GetDevice.
            let (dev, loopback) = unsafe {
                match endpoint {
                    AudioEndpoint::DefaultOutput => (e.GetDefaultAudioEndpoint(eRender, eConsole).ctx("no playback device")?, true),
                    AudioEndpoint::DefaultInput => (e.GetDefaultAudioEndpoint(eCapture, eConsole).ctx("no microphone")?, false),
                    AudioEndpoint::Device { id, loopback } => {
                        let w = to_wide(id);
                        (e.GetDevice(PCWSTR(w.as_ptr())).ctx("the audio device is not connected")?, *loopback)
                    }
                    AudioEndpoint::Process { .. } => return Err(PlatformError("unreachable endpoint".into())),
                }
            };
            // SAFETY: activates the audio client on a live device.
            let client: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None) }.ctx("cannot open the audio device")?;
            (client, loopback, device_id(&dev), device_name(&dev))
        }
    };
    let process = matches!(endpoint, AudioEndpoint::Process { .. });
    let base_flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK | if loopback { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
    let desired = float_stereo_48k();
    let buffer = if process { 0 } else { 2_000_000 }; // 200 ms device buffer
    // SAFETY: `desired` is a complete WAVEFORMATEXTENSIBLE that outlives the call.
    let direct = unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            base_flags | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            buffer,
            0,
            &desired.Format,
            None,
        )
    };
    let (rate, channels, kind) = match direct {
        Ok(()) => (48_000, 2, SampleKind::F32),
        Err(first) if !process => {
            // SAFETY: GetMixFormat returns a CoTaskMemAlloc'ed format, parsed, used and freed here.
            unsafe {
                let mix = client.GetMixFormat().map_err(|_| PlatformError(format!("the audio device refused the format: {first}")))?;
                let parsed = parse_format(mix);
                let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, base_flags, buffer, 0, mix, None);
                CoTaskMemFree(Some(mix as *const _));
                r.ctx("cannot start the audio device")?;
                parsed?
            }
        }
        Err(e) => return Err(PlatformError(format!("cannot start app audio capture: {e}"))),
    };
    // SAFETY: auto-reset unnamed event; handed to the client, closed by Stream::drop.
    let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.ctx("CreateEventW")?;
    // SAFETY: documented calls on the initialised client.
    let capture: IAudioCaptureClient = unsafe {
        if let Err(e) = client.SetEventHandle(event) {
            let _ = CloseHandle(event);
            return Err(PlatformError(format!("SetEventHandle: {e}")));
        }
        match client.GetService() {
            Ok(c) => c,
            Err(e) => {
                let _ = CloseHandle(event);
                return Err(PlatformError(format!("IAudioCaptureClient: {e}")));
            }
        }
    };
    let stream = Stream { client, capture, event, rate, channels, kind, device_id, name };
    // SAFETY: starts the configured client.
    unsafe { stream.client.Start() }.ctx("cannot start the audio device")?;
    Ok(stream)
}

/// A running capture; stops (and joins its thread) on drop.
pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<AudioCaptureState>>,
    dropped: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl AudioCapture {
    /// Starts capturing `endpoint` as mixer source `source`. Never fails because a device is
    /// missing: the state says so and the capture keeps retrying.
    pub fn start(endpoint: AudioEndpoint, source: SourceId, sink: Sender<AudioChunk>) -> crate::Result<AudioCapture> {
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(AudioCaptureState::Starting));
        let dropped = Arc::new(AtomicU64::new(0));
        let (stop2, state2, dropped2) = (Arc::clone(&stop), Arc::clone(&state), Arc::clone(&dropped));
        let thread = std::thread::Builder::new()
            .name("fluxcapture-audio".into())
            .spawn(move || {
                let r = crate::guarded("audio capture", || capture_loop(&endpoint, source, &sink, &stop2, &state2, &dropped2));
                let final_state = match r {
                    Ok(()) => AudioCaptureState::Stopped,
                    Err(e) => AudioCaptureState::Failed(e.0),
                };
                *state2.lock().unwrap_or_else(PoisonError::into_inner) = final_state;
            })
            .map_err(|e| PlatformError(format!("cannot start the audio thread: {e}")))?;
        Ok(AudioCapture { stop, state, dropped, thread: Some(thread) })
    }

    pub fn state(&self) -> AudioCaptureState {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Chunks dropped because the mixer did not keep up (its queue was full).
    pub fn dropped_chunks(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn set_state(state: &Mutex<AudioCaptureState>, s: AudioCaptureState) {
    *state.lock().unwrap_or_else(PoisonError::into_inner) = s;
}

fn capture_loop(
    endpoint: &AudioEndpoint,
    source: SourceId,
    sink: &Sender<AudioChunk>,
    stop: &AtomicBool,
    state: &Mutex<AudioCaptureState>,
    dropped: &AtomicU64,
) -> crate::Result<()> {
    ensure_com()?;
    let _prio = boost_thread(ThreadKind::ProAudio);
    let mut discontinuity = true;
    while !stop.load(Ordering::Relaxed) {
        match open(endpoint) {
            Ok(stream) => {
                set_state(state, AudioCaptureState::Running { device: stream.name.clone() });
                match pump(&stream, endpoint, source, sink, stop, dropped, &mut discontinuity) {
                    Ok(()) => {}
                    Err(e) => {
                        log::warn!("audio capture of {}: {e}", stream.name);
                        set_state(state, AudioCaptureState::Reconnecting(e.0));
                    }
                }
            }
            Err(e) => set_state(state, AudioCaptureState::Reconnecting(e.0)),
        }
        discontinuity = true;
        // Wait a second (interruptible) before reopening.
        for _ in 0..20 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(())
}

/// Reads packets until stopped, the device fails, or the default device changes (`Ok` to reopen).
fn pump(
    s: &Stream,
    endpoint: &AudioEndpoint,
    source: SourceId,
    sink: &Sender<AudioChunk>,
    stop: &AtomicBool,
    dropped: &AtomicU64,
    discontinuity: &mut bool,
) -> crate::Result<()> {
    let follows_default = matches!(endpoint, AudioEndpoint::DefaultOutput | AudioEndpoint::DefaultInput);
    let mut last_default_check = Instant::now();
    let channels = usize::from(s.channels);
    let format = AudioFormat { sample_rate: s.rate, channels: s.channels };
    while !stop.load(Ordering::Relaxed) {
        // SAFETY: waits on the stream's own event.
        let w = unsafe { WaitForSingleObject(s.event, 100) };
        let _ = w == WAIT_OBJECT_0;
        loop {
            // SAFETY: plain query on a started capture client.
            let n = unsafe { s.capture.GetNextPacketSize() }.map_err(|e| PlatformError(format!("audio device lost: {e}")))?;
            if n == 0 {
                break;
            }
            let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut::<u8>(), 0u32, 0u32, 0u64);
            // SAFETY: live out-locals; the buffer is released below in every path.
            unsafe { s.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)) }
                .map_err(|e| PlatformError(format!("audio read failed: {e}")))?;
            let count = frames as usize * channels;
            let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null();
            let mut samples = vec![0f32; if count <= 48_000 * 32 { count } else { 0 }];
            if !silent && !samples.is_empty() {
                // SAFETY: WASAPI guarantees `frames * block_align` readable bytes at `data` until
                // ReleaseBuffer; block_align = channels * sample size for the parsed format.
                unsafe { convert(data, s.kind, &mut samples) };
            }
            // SAFETY: releases exactly what GetBuffer returned.
            unsafe { s.capture.ReleaseBuffer(frames) }.map_err(|e| PlatformError(format!("audio release failed: {e}")))?;
            if frames == 0 {
                continue;
            }
            let chunk = AudioChunk {
                source,
                time: Hns(qpc.min(i64::MAX as u64) as i64),
                format,
                samples,
                discontinuity: *discontinuity || (flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0,
                silent,
            };
            *discontinuity = false;
            match sink.try_send(chunk) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    dropped.fetch_add(1, Ordering::Relaxed);
                    *discontinuity = true;
                }
                Err(TrySendError::Disconnected(_)) => return Ok(()),
            }
        }
        if follows_default && last_default_check.elapsed() > Duration::from_secs(1) {
            last_default_check = Instant::now();
            let flow = if matches!(endpoint, AudioEndpoint::DefaultOutput) { eRender } else { eCapture };
            if let Ok(e) = enumerator()
                // SAFETY: plain query.
                && let Ok(d) = unsafe { e.GetDefaultAudioEndpoint(flow, eConsole) }
                && device_id(&d) != s.device_id
            {
                log::info!("default audio device changed; switching");
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Converts interleaved device samples to f32.
///
/// # Safety
/// `data` must hold `out.len()` samples of `kind`.
unsafe fn convert(data: *const u8, kind: SampleKind, out: &mut [f32]) {
    let n = out.len();
    // SAFETY: per the contract, each arm reads exactly `n` samples of its size.
    unsafe {
        match kind {
            SampleKind::F32 => {
                let src = std::slice::from_raw_parts(data.cast::<u8>(), n * 4);
                for (o, b) in out.iter_mut().zip(src.as_chunks::<4>().0) {
                    *o = f32::from_le_bytes(*b);
                }
            }
            SampleKind::I16 => {
                let src = std::slice::from_raw_parts(data, n * 2);
                for (o, b) in out.iter_mut().zip(src.as_chunks::<2>().0) {
                    *o = f32::from(i16::from_le_bytes(*b)) / 32768.0;
                }
            }
            SampleKind::I24 => {
                let src = std::slice::from_raw_parts(data, n * 3);
                for (o, b) in out.iter_mut().zip(src.as_chunks::<3>().0) {
                    *o = (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0;
                }
            }
            SampleKind::I32 => {
                let src = std::slice::from_raw_parts(data, n * 4);
                for (o, b) in out.iter_mut().zip(src.as_chunks::<4>().0) {
                    *o = i32::from_le_bytes(*b) as f32 / 2_147_483_648.0;
                }
            }
        }
    }
}

/// The Media Foundation AAC-LC encoder (48 kHz stereo, 16-bit PCM in, raw AAC frames out).
pub struct AacEncoder {
    transform: IMFTransform,
    out_size: u32,
    sample_rate: u32,
    channels: u16,
    /// Media time of the next input sample.
    next_time: Option<Hns>,
}

// SAFETY: the Microsoft AAC encoder is a synchronous, free-threaded MFT; the encoder is used by one
// thread at a time (it moves to the mixer thread once, then stays there).
unsafe impl Send for AacEncoder {}

impl AacEncoder {
    /// Bitrates the encoder accepts, kbit/s.
    pub const BITRATES: [u32; 4] = [96, 128, 160, 192];

    pub fn new(sample_rate: u32, channels: u16, kbps: u32) -> crate::Result<AacEncoder> {
        ensure_com()?;
        if !matches!(sample_rate, 44_100 | 48_000) || !matches!(channels, 1 | 2 | 6) {
            return Err(PlatformError(format!("AAC: unsupported format {sample_rate} Hz / {channels} channels")));
        }
        let kbps = *Self::BITRATES.iter().min_by_key(|b| (i64::from(**b) - i64::from(kbps)).abs()).unwrap_or(&160);
        let api = mf::api()?;
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_AAC };
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_PCM };
        let list =
            api.enum_transforms(MFT_CATEGORY_AUDIO_ENCODER, (MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0) as u32, Some(input), Some(output), None)?;
        let activate = list.first().ok_or_else(|| PlatformError("no AAC encoder installed (Media Feature Pack missing?)".into()))?;
        // SAFETY: documented MFT configuration calls on objects created here.
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().ctx("cannot create the AAC encoder")?;
            let out = api.media_type()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).ctx("AAC type")?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC).ctx("AAC type")?;
            out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).ctx("AAC type")?;
            out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate).ctx("AAC type")?;
            out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(channels)).ctx("AAC type")?;
            out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, kbps * 1000 / 8).ctx("AAC type")?;
            out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).ctx("AAC type")?;
            out.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29).ctx("AAC type")?;
            transform.SetOutputType(0, &out, 0).ctx("AAC output format refused")?;
            let inp = api.media_type()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).ctx("PCM type")?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).ctx("PCM type")?;
            inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).ctx("PCM type")?;
            inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate).ctx("PCM type")?;
            inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(channels)).ctx("PCM type")?;
            inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, u32::from(channels) * 2).ctx("PCM type")?;
            inp.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, sample_rate * u32::from(channels) * 2).ctx("PCM type")?;
            transform.SetInputType(0, &inp, 0).ctx("AAC input format refused")?;
            let si = transform.GetOutputStreamInfo(0).ctx("AAC stream info")?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).ctx("AAC begin")?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).ctx("AAC start")?;
            Ok(AacEncoder { transform, out_size: si.cbSize.max(8192), sample_rate, channels, next_time: None })
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Encodes interleaved PCM starting at media time `time`; finished frames are appended to `out`.
    pub fn encode(&mut self, pcm: &[i16], time: Hns, out: &mut Vec<EncodedAudio>) -> crate::Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        let api = mf::api()?;
        let bytes = u32::try_from(pcm.len() * 2).map_err(|_| "audio block too large")?;
        let buffer = api.memory_buffer(bytes)?;
        let frames = pcm.len() / usize::from(self.channels.max(1));
        let duration = Hns(frames as i64 * 10_000_000 / i64::from(self.sample_rate.max(1)));
        self.next_time.get_or_insert(time);
        // SAFETY: the buffer has `bytes` writable bytes while locked; we copy exactly that many.
        unsafe {
            let (mut p, mut max) = (std::ptr::null_mut::<u8>(), 0u32);
            buffer.Lock(&mut p, Some(&mut max), None).ctx("AAC input lock")?;
            if p.is_null() || max < bytes {
                let _ = buffer.Unlock();
                return Err(PlatformError("AAC input buffer too small".into()));
            }
            std::ptr::copy_nonoverlapping(pcm.as_ptr().cast::<u8>(), p, bytes as usize);
            buffer.Unlock().ctx("AAC input unlock")?;
            buffer.SetCurrentLength(bytes).ctx("AAC input length")?;
            let sample = api.sample()?;
            sample.AddBuffer(&buffer).ctx("AAC input")?;
            sample.SetSampleTime(time.0).ctx("AAC input")?;
            sample.SetSampleDuration(duration.0).ctx("AAC input")?;
            self.transform.ProcessInput(0, &sample, 0).ctx("AAC encode")?;
        }
        self.drain_into(out)
    }

    /// Flushes the encoder's last frames.
    pub fn finish(&mut self, out: &mut Vec<EncodedAudio>) -> crate::Result<()> {
        // SAFETY: plain message.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }.ctx("AAC drain")?;
        self.drain_into(out)
    }

    fn drain_into(&mut self, out: &mut Vec<EncodedAudio>) -> crate::Result<()> {
        let api = mf::api()?;
        loop {
            let sample = api.sample()?;
            let buffer = api.memory_buffer(self.out_size)?;
            // SAFETY: plain COM call.
            unsafe { sample.AddBuffer(&buffer) }.ctx("AAC output")?;
            let mut db = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(Some(sample)), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
            let mut status = 0;
            // SAFETY: one descriptor; the references it holds are taken back below.
            let r = unsafe { self.transform.ProcessOutput(0, &mut db, &mut status) };
            // SAFETY: each field is taken exactly once.
            let (sample, _ev) = unsafe { (ManuallyDrop::take(&mut db[0].pSample), ManuallyDrop::take(&mut db[0].pEvents)) };
            match r {
                Ok(()) => {
                    let Some(sample) = sample else { continue };
                    // SAFETY: reads a finished output sample; the buffer is locked while copying the
                    // `len` valid bytes.
                    let frame = unsafe {
                        let t = sample.GetSampleTime().unwrap_or(0);
                        let d = sample.GetSampleDuration().unwrap_or(1024 * 10_000_000 / i64::from(self.sample_rate.max(1)));
                        let b = sample.ConvertToContiguousBuffer().ctx("AAC output")?;
                        let (mut p, mut len) = (std::ptr::null_mut::<u8>(), 0u32);
                        b.Lock(&mut p, None, Some(&mut len)).ctx("AAC output lock")?;
                        let data = if p.is_null() { Vec::new() } else { std::slice::from_raw_parts(p, len as usize).to_vec() };
                        let _ = b.Unlock();
                        EncodedAudio { pts: Hns(t), duration: Hns(d), data }
                    };
                    if !frame.data.is_empty() {
                        out.push(frame);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(PlatformError(format!("AAC output failed: {e}"))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_and_aac() {
        // Device lists may be empty on CI machines; they must not fail.
        let _ = list_audio_devices().unwrap();
        let mut enc = AacEncoder::new(48_000, 2, 160).unwrap();
        let mut out = Vec::new();
        let tone: Vec<i16> = (0..48_000 * 2).map(|i| ((i as f32 / 2.0 * 0.0573).sin() * 8000.0) as i16).collect();
        for (i, block) in tone.chunks(960).enumerate() {
            enc.encode(block, Hns(i as i64 * 100_000), &mut out).unwrap();
        }
        enc.finish(&mut out).unwrap();
        // One second at 48 kHz ≈ 47 AAC frames of 1024 samples.
        assert!((44..=50).contains(&out.len()), "{} frames", out.len());
        assert!(out.windows(2).all(|w| w[1].pts > w[0].pts));
        let total: usize = out.iter().map(|f| f.data.len()).sum();
        assert!(total > 10_000 && total < 40_000, "{total} bytes");
    }

    #[test]
    fn format_parsing_rejects_garbage() {
        let mut f = WAVEFORMATEX { wFormatTag: 1, nChannels: 0, nSamplesPerSec: 48_000, wBitsPerSample: 16, ..Default::default() };
        // SAFETY: a complete WAVEFORMATEX with cbSize 0.
        assert!(unsafe { parse_format(&f) }.is_err());
        f.nChannels = 2;
        // SAFETY: as above.
        assert_eq!(unsafe { parse_format(&f) }.unwrap(), (48_000, 2, SampleKind::I16));
        f.wBitsPerSample = 12;
        // SAFETY: as above.
        assert!(unsafe { parse_format(&f) }.is_err());
    }
}
