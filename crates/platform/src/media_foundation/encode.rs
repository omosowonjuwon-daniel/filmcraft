//! Hardware H.264 export through a Media Foundation encoder MFT: Intel Quick Sync (and any other
//! vendor's asynchronous hardware encoder MFT) on Windows. [`factory`] is registered behind NVENC,
//! in front of the software encoder, and takes an export only when Export ▸ Hardware encoding is
//! Auto and an MFT opens for the settings; otherwise it returns `None` and the software encoder
//! runs, exactly as before.
//!
//! The MFT gets system-memory NV12 pictures (converted on the CPU with the same BT.709
//! limited-range codes as the software encoder) and returns Annex B access units, which are
//! rewritten with 4-byte lengths for MP4. B-frames are turned off, so every packet's composition
//! offset is zero; an MFT that reorders anyway ends the export with an error that says how to fall
//! back.
//!
//! FFI module (docs/adr/0001-platform-ffi.md): every `unsafe` block has a `// SAFETY:` comment.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use filmcraft_export::{BitrateMode, EncodedPacket, EncoderFrame, ExportError, ExportSettings, FieldOrder, Format, H264Pass, H264Profile, HardwareEncoding, Result, VideoEncoder};
use filmcraft_isobmff::{AvcConfig, SampleEntry};
use filmcraft_time::FrameRate;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, ICodecAPI, IMFActivate, IMFMediaEventGenerator, IMFMediaType, IMFTransform, METransformDrainComplete, METransformHaveOutput,
    METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE, MF_E_NO_MORE_TYPES, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MFMediaType_Video, MFSampleExtension_CleanPoint, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive, eAVEncCommonRateControlMode_CBR,
    eAVEncCommonRateControlMode_PeakConstrainedVBR, eAVEncH264VProfile_Base, eAVEncH264VProfile_High, eAVEncH264VProfile_Main,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::Interface;

use super::gpu::ensure_com;
use super::mft::{MfApi, api, friendly_name};

/// How long the MFT may go without an event before the export gives up on it (a hung driver).
const EVENT_TIMEOUT: Duration = Duration::from_secs(20);
/// Media Foundation time: 100 ns units.
const MF_SECOND: i128 = 10_000_000;

/// What the export asks of the MFT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Config {
    width: u32,
    height: u32,
    fps: (u32, u32),
    bitrate_kbps: u32,
    max_bitrate_kbps: u32,
    cbr: bool,
    keyint: u32,
    profile: H264Profile,
}

/// Why the MFT does not take this export (the software encoder does), or its configuration.
fn config(format: Format, w: u32, h: u32, rate: FrameRate, s: &ExportSettings) -> std::result::Result<Config, String> {
    if format != Format::H264 || s.format.is_mxf() {
        return Err("only MP4 / MOV H.264 exports".into());
    }
    if s.signal.is_hdr() {
        return Err("HDR".into());
    }
    if !matches!(s.h264_pass, H264Pass::Single) || s.bitrate_mode == BitrateMode::Vbr2Pass {
        return Err("two-pass VBR".into());
    }
    if s.bitrate_mode == BitrateMode::Crf {
        return Err("CRF (constant quality)".into());
    }
    if s.pixel_aspect.is_some_and(|(n, d)| n != d) {
        return Err("non-square pixels".into());
    }
    if s.field_order != FieldOrder::Progressive {
        return Err("interlaced output".into());
    }
    if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err("the encoder needs nonzero even dimensions".into());
    }
    if w > 8192 || h > 8192 {
        return Err(format!("{w}x{h} is larger than hardware H.264 encoders take"));
    }
    let (Ok(num), Ok(den)) = (u32::try_from(rate.num), u32::try_from(rate.den)) else {
        return Err("frame rate".into());
    };
    if num == 0 || den == 0 {
        return Err("frame rate".into());
    }
    let kbps = s.bitrate_kbps.max(100);
    Ok(Config {
        width: w,
        height: h,
        fps: (num, den),
        bitrate_kbps: kbps,
        max_bitrate_kbps: s.max_bitrate_kbps.filter(|m| *m >= kbps).unwrap_or_else(|| kbps.saturating_add(kbps / 2)),
        cbr: s.bitrate_mode == BitrateMode::Cbr,
        keyint: s.keyframe_distance.filter(|k| *k > 0).unwrap_or_else(|| (f64::from(num) / f64::from(den) * 2.0).round().clamp(1.0, 600.0) as u32),
        profile: s.h264_profile,
    })
}

/// The Export encoder factory (see the module documentation).
pub fn factory(format: Format, w: u32, h: u32, rate: FrameRate, s: &ExportSettings) -> Option<Result<Box<dyn VideoEncoder>>> {
    if s.hardware_encoding != HardwareEncoding::Auto || format != Format::H264 {
        return None;
    }
    let declined = |why: &str| {
        log::info!("Media Foundation hardware encoding declined: {why}");
        filmcraft_export::note_hw_encode_declined();
        None
    };
    let cfg = match config(format, w, h, rate, s) {
        Ok(c) => c,
        Err(why) => return declined(&why),
    };
    match MfEncoder::open(&cfg) {
        Ok(enc) => {
            log::info!("hardware encoding: {} ({}x{}, {} kb/s)", enc.name, cfg.width, cfg.height, cfg.bitrate_kbps);
            filmcraft_export::note_hw_encode_session();
            Some(Ok(Box::new(enc)))
        }
        Err(why) => declined(&why),
    }
}

/// What one `ProcessOutput` call gave.
enum Pulled {
    Packet(Option<EncodedPacket>),
    StreamChange,
}

/// An asynchronous hardware H.264 encoder MFT, configured for one export.
struct MfEncoder {
    api: &'static MfApi,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    name: String,
    cfg: Config,
    /// Whether the MFT hands out its own output samples (else one of `out_size` bytes is given).
    provides_samples: bool,
    out_size: u32,
    /// METransformNeedInput events not yet answered with a picture.
    credits: u32,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    /// The time of the last packet, to notice reordering.
    last_time: Option<i64>,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    nv12: Vec<u8>,
}

// SAFETY: Media Foundation transforms and their event generators are free-threaded COM objects;
// an `MfEncoder` is only used through `&mut` by the one export that owns it.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    fn open(cfg: &Config) -> std::result::Result<MfEncoder, String> {
        ensure_com()?;
        let api = api()?;
        let candidates = api.hardware_encoders(MFVideoFormat_H264)?;
        if candidates.is_empty() {
            return Err("no hardware H.264 encoder MFT on this machine".into());
        }
        let mut reasons = Vec::new();
        for a in candidates {
            match Self::configure(api, &a, cfg) {
                Ok(e) => return Ok(e),
                Err(e) => reasons.push(e),
            }
        }
        Err(reasons.join("; "))
    }

    fn configure(api: &'static MfApi, activate: &IMFActivate, cfg: &Config) -> std::result::Result<MfEncoder, String> {
        let name = friendly_name(activate).unwrap_or_else(|| "a hardware encoder MFT".into());
        let fail = |what: &str, e: windows::core::Error| format!("{name}: {what}: {e}");
        // SAFETY: plain COM calls on live interfaces.
        let transform: IMFTransform = unsafe { activate.ActivateObject() }.map_err(|e| fail("cannot be created", e))?;
        // SAFETY: plain COM calls on live interfaces.
        unsafe {
            let attrs = transform.GetAttributes().map_err(|e| fail("no attributes", e))?;
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                return Err(format!("{name}: not an asynchronous (hardware) MFT"));
            }
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(|e| fail("cannot be unlocked", e))?;
        }
        let events: IMFMediaEventGenerator = transform.cast().map_err(|e| fail("has no event generator", e))?;
        // Rate control and GOP are set before the media types, which some MFTs only read once.
        if let Ok(codec) = transform.cast::<ICodecAPI>() {
            let mode = if cfg.cbr { eAVEncCommonRateControlMode_CBR.0 } else { eAVEncCommonRateControlMode_PeakConstrainedVBR.0 };
            let values = [
                (CODECAPI_AVEncCommonRateControlMode, mode as u32),
                (CODECAPI_AVEncCommonMeanBitRate, cfg.bitrate_kbps.saturating_mul(1000)),
                (CODECAPI_AVEncCommonMaxBitRate, cfg.max_bitrate_kbps.saturating_mul(1000)),
                (CODECAPI_AVEncMPVGOPSize, cfg.keyint),
                (CODECAPI_AVEncMPVDefaultBPictureCount, 0),
            ];
            for (key, value) in values {
                let v = VARIANT::from(value);
                // SAFETY: a plain COM call with a live GUID and VARIANT.
                if let Err(e) = unsafe { codec.SetValue(&key, &v) } {
                    // the MFT keeps its default for that one (logged; the stream is still valid)
                    log::info!("{name}: codec property {key:?} not set: {e}");
                }
            }
        }
        let out = Self::output_type(api, cfg).map_err(|e| format!("{name}: {e}"))?;
        // SAFETY: plain COM calls on live interfaces.
        unsafe { transform.SetOutputType(0, &out, 0) }.map_err(|e| fail("refused the H.264 output type", e))?;
        let input = Self::input_type(&transform, cfg).map_err(|e| format!("{name}: {e}"))?;
        // SAFETY: plain COM calls on live interfaces.
        unsafe { transform.SetInputType(0, &input, 0) }.map_err(|e| fail("refused NV12 input", e))?;
        // SAFETY: plain COM calls on live interfaces.
        let info = unsafe { transform.GetOutputStreamInfo(0) }.map_err(|e| fail("no output stream info", e))?;
        // SAFETY: plain COM calls on live interfaces.
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(|e| fail("cannot begin streaming", e))?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(|e| fail("cannot start the stream", e))?;
        }
        Ok(MfEncoder {
            api,
            transform,
            events,
            name,
            cfg: *cfg,
            provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
            out_size: info.cbSize.max(1 << 20),
            credits: 0,
            sps: None,
            pps: None,
            last_time: None,
            y: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
            nv12: Vec::new(),
        })
    }

    fn output_type(api: &MfApi, cfg: &Config) -> std::result::Result<IMFMediaType, String> {
        let t = api.media_type()?;
        let profile = match cfg.profile {
            H264Profile::Baseline => eAVEncH264VProfile_Base.0,
            H264Profile::Main => eAVEncH264VProfile_Main.0,
            H264Profile::High => eAVEncH264VProfile_High.0,
        };
        // SAFETY: plain COM calls on a live interface.
        unsafe {
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(|e| e.to_string())?;
            t.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_kbps.saturating_mul(1000)).map_err(|e| e.to_string())?;
            Self::set_video(&t, cfg)?;
            t.SetUINT32(&MF_MT_MPEG2_PROFILE, profile as u32).map_err(|e| e.to_string())?;
        }
        Ok(t)
    }

    /// The MFT's own NV12 input type, sized for the export.
    fn input_type(transform: &IMFTransform, cfg: &Config) -> std::result::Result<IMFMediaType, String> {
        for i in 0..64 {
            // SAFETY: plain COM calls on live interfaces.
            unsafe {
                match transform.GetInputAvailableType(0, i) {
                    Ok(t) => {
                        if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                            Self::set_video(&t, cfg)?;
                            return Ok(t);
                        }
                    }
                    Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
                    Err(e) => return Err(format!("no input types: {e}")),
                }
            }
        }
        Err("does not take NV12 input".into())
    }

    /// Size, frame rate, square pixels and progressive scan on `t`.
    fn set_video(t: &IMFMediaType, cfg: &Config) -> std::result::Result<(), String> {
        // SAFETY: plain COM calls on a live interface.
        unsafe {
            t.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(cfg.width) << 32) | u64::from(cfg.height)).map_err(|e| e.to_string())?;
            t.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(cfg.fps.0) << 32) | u64::from(cfg.fps.1)).map_err(|e| e.to_string())?;
            t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1).map_err(|e| e.to_string())?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Picture `index`'s presentation time in 100 ns units.
    fn time_of(&self, index: u64) -> i64 {
        let (num, den) = (i128::from(self.cfg.fps.0.max(1)), i128::from(self.cfg.fps.1));
        let t = i128::from(index).saturating_mul(MF_SECOND).saturating_mul(den) / num;
        i64::try_from(t).unwrap_or(i64::MAX)
    }

    fn err(&self, what: &str) -> ExportError {
        ExportError::Encode(format!("{}: {what} (turn Export > Hardware encoding off to use the software encoder)", self.name))
    }

    /// Handle the MFT's next event: count input requests, collect output. `Ok(false)` once it
    /// says draining is complete. With `wait` it polls until an event arrives (or the timeout).
    fn next_event(&mut self, wait: bool, out: &mut Vec<EncodedPacket>) -> Result<Option<bool>> {
        let start = Instant::now();
        let event = loop {
            // SAFETY: a plain COM call on a live interface; NO_WAIT returns at once.
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => break e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if !wait {
                        return Ok(None);
                    }
                    if start.elapsed() > EVENT_TIMEOUT {
                        return Err(self.err("the hardware encoder stopped responding"));
                    }
                    std::thread::sleep(Duration::from_micros(500));
                }
                Err(e) => return Err(self.err(&format!("event queue failed: {e}"))),
            }
        };
        // SAFETY: a plain COM call on a live interface.
        let kind = unsafe { event.GetType() }.map_err(|e| self.err(&format!("unreadable event: {e}")))?;
        if kind == METransformNeedInput.0 as u32 {
            self.credits = self.credits.saturating_add(1);
        } else if kind == METransformHaveOutput.0 as u32 {
            if let Some(p) = self.pull()? {
                out.push(p);
            }
        } else if kind == METransformDrainComplete.0 as u32 {
            return Ok(Some(false));
        }
        Ok(Some(true))
    }

    /// Take one encoded access unit from the MFT, renegotiating its output type when it asks
    /// (Intel's MFT does so before its first packet, once it has chosen the stream's parameters).
    /// After a renegotiation the MFT sends a new METransformHaveOutput for the same packet; an
    /// asynchronous MFT must not be asked for output without one.
    fn pull(&mut self) -> Result<Option<EncodedPacket>> {
        match self.pull_once()? {
            Pulled::Packet(p) => Ok(p),
            Pulled::StreamChange => {
                self.renegotiate()?;
                Ok(None)
            }
        }
    }

    /// Set the output type the MFT now offers and re-read its output stream info.
    fn renegotiate(&mut self) -> Result<()> {
        // SAFETY: plain COM calls on live interfaces.
        unsafe {
            let t = self.transform.GetOutputAvailableType(0, 0).map_err(|e| self.err(&format!("no output type after a format change: {e}")))?;
            self.transform.SetOutputType(0, &t, 0).map_err(|e| self.err(&format!("refused its own output type: {e}")))?;
            let info = self.transform.GetOutputStreamInfo(0).map_err(|e| self.err(&format!("no output stream info: {e}")))?;
            self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            self.out_size = info.cbSize.max(1 << 20);
        }
        log::info!("{}: output format renegotiated", self.name);
        Ok(())
    }

    fn pull_once(&mut self) -> Result<Pulled> {
        let given = if self.provides_samples {
            None
        } else {
            let sample = self.api.sample_of(&vec![0u8; self.out_size as usize], 0).map_err(|e| self.err(&e))?;
            Some(sample)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(given), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
        let mut status = 0u32;
        // SAFETY: one output descriptor (with our sample, or none when the MFT provides its own)
        // and a live status out-pointer.
        let r = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
        // SAFETY: the descriptor's fields were initialised above and are taken once; dropping them
        // releases the references the MFT handed over.
        let (sample, _events) = unsafe { (ManuallyDrop::take(&mut buffers[0].pSample), ManuallyDrop::take(&mut buffers[0].pEvents)) };
        match r {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(Pulled::Packet(None)),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => return Ok(Pulled::StreamChange),
            Err(e) => return Err(self.err(&format!("ProcessOutput failed: {e}"))),
        }
        let Some(sample) = sample else { return Ok(Pulled::Packet(None)) };
        // SAFETY: plain COM calls on live interfaces; the locked block holds `len` readable bytes
        // until `Unlock`, and is copied out before that.
        let (bytes, time, clean) = unsafe {
            let buffer = sample.ConvertToContiguousBuffer().map_err(|e| self.err(&format!("no output buffer: {e}")))?;
            let (mut ptr, mut len) = (std::ptr::null_mut::<u8>(), 0u32);
            buffer.Lock(&mut ptr, None, Some(&mut len)).map_err(|e| self.err(&format!("cannot read the output: {e}")))?;
            let bytes = if ptr.is_null() { Vec::new() } else { std::slice::from_raw_parts(ptr, len as usize).to_vec() };
            let _ = buffer.Unlock();
            (bytes, sample.GetSampleTime().ok(), sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0)
        };
        if let (Some(t), Some(last)) = (time, self.last_time)
            && t < last
        {
            return Err(self.err("the encoder reordered pictures (B-frames could not be turned off)"));
        }
        self.last_time = time.or(self.last_time);
        let au = split_access_unit(&bytes);
        if self.sps.is_none() {
            self.sps = au.sps.clone();
        }
        if self.pps.is_none() {
            self.pps = au.pps.clone();
        }
        if au.data.is_empty() {
            return Ok(Pulled::Packet(None));
        }
        filmcraft_export::note_hw_encode_frame();
        Ok(Pulled::Packet(Some(EncodedPacket { data: au.data, key: clean || au.idr, duration: self.cfg.fps.1, composition_offset: 0 })))
    }

    /// The SPS and PPS from the MFT's current output type (`MF_MT_MPEG_SEQUENCE_HEADER`), for an
    /// MFT that keeps them out of the stream.
    fn header_parameter_sets(&self) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        // SAFETY: plain COM calls on live interfaces; `GetBlob` writes at most the buffer's length.
        let blob = unsafe {
            let Ok(t) = self.transform.GetOutputCurrentType(0) else { return (None, None) };
            let Ok(size) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else { return (None, None) };
            let mut b = vec![0u8; (size as usize).min(1 << 16)];
            let mut len = 0u32;
            if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut b, Some(&mut len)).is_err() {
                return (None, None);
            }
            b.truncate(len as usize);
            b
        };
        let au = split_access_unit(&blob);
        (au.sps, au.pps)
    }
}

impl VideoEncoder for MfEncoder {
    fn sample_entry(&self) -> SampleEntry {
        let (mut sps, mut pps) = (self.sps.clone(), self.pps.clone());
        if sps.is_none() || pps.is_none() {
            let (s, p) = self.header_parameter_sets();
            sps = sps.or(s);
            pps = pps.or(p);
        }
        let (w, h) = (u16::try_from(self.cfg.width).unwrap_or(u16::MAX), u16::try_from(self.cfg.height).unwrap_or(u16::MAX));
        SampleEntry::avc(AvcConfig::new(sps.into_iter().collect(), pps.into_iter().collect(), 4), w, h)
    }

    fn timescale(&self) -> u32 {
        self.cfg.fps.0
    }

    fn encode(&mut self, f: &EncoderFrame) -> Result<Vec<EncodedPacket>> {
        ensure_com().map_err(|e| self.err(&e))?;
        if f.width != self.cfg.width || f.height != self.cfg.height {
            return Err(self.err(&format!("a {}x{} picture for a {}x{} encoder", f.width, f.height, self.cfg.width, self.cfg.height)));
        }
        if f.hdr.is_some() {
            return Err(ExportError::Unsupported("the Media Foundation encoder is 8-bit SDR".into()));
        }
        let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
        filmcraft_export::timed(filmcraft_export::Stage::Convert, || {
            filmcraft_export::rgba_to_yuv420_8(f.rgba, w, h, &mut self.y, &mut self.u, &mut self.v);
            interleave_nv12(&self.y, &self.u, &self.v, &mut self.nv12);
        });
        let mut out = Vec::new();
        while self.credits == 0 {
            self.next_event(true, &mut out)?;
        }
        let time = self.time_of(f.index);
        let duration = self.time_of(f.index.saturating_add(1)).saturating_sub(time);
        let sample = self.api.sample_of(&self.nv12, time).map_err(|e| self.err(&e))?;
        // SAFETY: plain COM calls on live interfaces.
        unsafe {
            let _ = sample.SetSampleDuration(duration);
            self.transform.ProcessInput(0, &sample, 0).map_err(|e| self.err(&format!("ProcessInput failed: {e}")))?;
        }
        self.credits = self.credits.saturating_sub(1);
        // collect what is ready without waiting, so packets leave as soon as they are encoded
        while self.next_event(false, &mut out)?.is_some() {}
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        ensure_com().map_err(|e| self.err(&e))?;
        // SAFETY: plain COM calls on a live interface.
        unsafe {
            self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0).map_err(|e| self.err(&format!("end of stream failed: {e}")))?;
            self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0).map_err(|e| self.err(&format!("drain failed: {e}")))?;
        }
        let mut out = Vec::new();
        while self.next_event(true, &mut out)? != Some(false) {}
        Ok(out)
    }
}

/// NV12 (one Y plane, then interleaved U/V) from planar 4:2:0.
fn interleave_nv12(y: &[u8], u: &[u8], v: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(y.len() + u.len() + v.len());
    out.extend_from_slice(y);
    for (a, b) in u.iter().zip(v) {
        out.push(*a);
        out.push(*b);
    }
}

/// An Annex B access unit taken apart for MP4.
#[derive(Debug, Default, PartialEq, Eq)]
struct AccessUnit {
    /// The NAL units other than parameter sets and delimiters, each behind a 4-byte length.
    data: Vec<u8>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    /// Holds an IDR slice.
    idr: bool,
}

/// The NAL unit payloads of an Annex B byte string (start codes of 3 or 4 bytes).
fn nal_units(b: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= b.len() {
        if b.get(i..i + 3) == Some(&[0, 0, 1]) {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(b.len(), |n| n - 3);
        // the zero bytes in front of the next start code (a 4-byte one, trailing zeros) are not payload
        while end > s && b.get(end - 1) == Some(&0) {
            end -= 1;
        }
        if let Some(nal) = b.get(s..end)
            && !nal.is_empty()
        {
            out.push(nal);
        }
    }
    out
}

fn split_access_unit(b: &[u8]) -> AccessUnit {
    let mut au = AccessUnit::default();
    for nal in nal_units(b) {
        let Some(&head) = nal.first() else { continue };
        match head & 0x1f {
            7 => {
                au.sps.get_or_insert_with(|| nal.to_vec());
            }
            8 => {
                au.pps.get_or_insert_with(|| nal.to_vec());
            }
            9 => {}
            t => {
                if t == 5 {
                    au.idr = true;
                }
                let Ok(len) = u32::try_from(nal.len()) else { continue };
                au.data.extend_from_slice(&len.to_be_bytes());
                au.data.extend_from_slice(nal);
            }
        }
    }
    au
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_parameter_sets_from_slices() {
        let b = [0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9, 9, 0];
        let au = split_access_unit(&b);
        assert_eq!(au.sps.as_deref(), Some(&[0x67, 1, 2][..]));
        assert_eq!(au.pps.as_deref(), Some(&[0x68, 3][..]));
        assert!(au.idr);
        assert_eq!(au.data, vec![0, 0, 0, 3, 0x65, 9, 9]);
    }

    #[test]
    fn non_idr_slice_is_not_key() {
        let au = split_access_unit(&[0, 0, 1, 0x41, 7, 7]);
        assert!(!au.idr);
        assert_eq!(au.data, vec![0, 0, 0, 3, 0x41, 7, 7]);
    }

    #[test]
    fn garbage_is_harmless() {
        for b in [&[][..], &[0, 0, 1][..], &[0, 0, 0, 0][..], &[1, 2, 3][..]] {
            let au = split_access_unit(b);
            assert!(au.data.is_empty());
        }
    }

    #[test]
    fn nv12_interleaves_chroma() {
        let mut out = Vec::new();
        interleave_nv12(&[1, 2, 3, 4], &[5], &[6], &mut out);
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn declines_what_it_cannot_write() {
        let rate = FrameRate { num: 30, den: 1 };
        let mut s = ExportSettings::default();
        assert!(config(Format::H264, 1920, 1080, rate, &s).is_ok());
        assert!(config(Format::Hevc, 1920, 1080, rate, &s).is_err());
        assert!(config(Format::H264, 1921, 1080, rate, &s).is_err());
        s.bitrate_mode = BitrateMode::Crf;
        assert!(config(Format::H264, 1920, 1080, rate, &s).is_err());
    }
}
