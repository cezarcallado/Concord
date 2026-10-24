//! Webcam capture (v0.3, item 11).
//!
//! ```text
//! Media Foundation source reader (async, D3D11 device manager)
//!   -> frame: a D3D11 texture (hardware decoder / D3D-aware source: stays on the GPU) or system
//!      memory (USB YUY2 / NV12, software MJPEG decode: one staging upload)
//!   -> video processor: camera YCbCr (BT.601/709, limited or JPEG full range) -> BT.709 NV12,
//!      scaled to fit the configured size (the same `Converter` as screen capture)
//!   -> main sink: the hardware encoder sink of the screen path (NVENC / MF), or an NV12
//!      readback for the software-encoder fallback, or nothing (preview only)
//!   -> preview sink (optional, switchable): a second, small NV12 conversion + readback for the
//!      local self-view, so our own stream is never decoded.
//! ```
//!
//! Threading: one thread owns the device, the reader and the sinks. The reader's async callback
//! only parks the newest sample and sets an event; the thread sleeps in `WaitForMultipleObjects`
//! on {stop, sample, encoder output, preview readback} with exactly one `ReadSample` in flight,
//! so a camera costs nothing between frames. Mode choice: the native mode that reaches the
//! wanted frame rate at (or just above) the wanted size, uncompressed before MJPEG at equal
//! size. Dropping the handle flushes the reader and shuts the media source down, which releases
//! the device (the camera light goes off).
//!
//! The synthetic camera (`SYNTHETIC_CAMERA_ID`) renders a cheap moving picture into the same
//! upload texture on a waitable timer, so everything after the device is exercised.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{implement, Interface, GUID, HRESULT, PWSTR};
use windows::Win32::Foundation::{E_FAIL, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_YUY2, DXGI_SAMPLE_DESC,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForMultipleObjects,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
};
use windows_core::PCWSTR;

use super::camctl::{Controls, CtlQueue};
use super::encode::{self, mf_startup, EncParams, EncShared, EncoderSink, Tuning};
use super::video::{self, Converter, Pace, Pacer, Readback, Shared, Sink, SinkCtx, Xform, YuvSpace};
use super::{monotonic_now_us, ComInit, Event, OwnedHandle};
use crate::{
    camera_geometry, CameraAdjust, CameraConfig, CameraControl, CameraControlId, CameraError, CameraFault,
    CameraFormat, CameraGeometry, CameraInfo, CameraMode, CameraRotation, CameraStats, CameraSubtype,
    CameraTransform, CaptureError, DegradationPreference, EncodedCallback, EncoderBackend, EncoderControl,
    FrameCallback, LowLightDetector, Result, SYNTHETIC_CAMERA_ID,
};

const FIRST_VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
const ALL_STREAMS: u32 = MF_SOURCE_READER_ALL_STREAMS.0 as u32;
/// How long the start waits for the first frame (cameras need ~0.3-1.5 s to start streaming).
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(6);
/// A running camera that delivers nothing for this long is reported as stopped.
const STALL_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONSECUTIVE_ERRORS: u32 = 30;
const STAGING_RING: usize = 3;

/// What the main sink does with the converted frames.
pub enum CameraOutput {
    Encoded(EncodedCallback),
    Raw(FrameCallback),
    /// Preview only (settings dialog).
    None,
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

fn cam_err(fault: CameraFault, detail: impl Into<String>) -> CameraError {
    CameraError { fault, detail: detail.into() }
}

/// Map a Media Foundation / Win32 failure to what the user can act on.
fn fault_of(hr: HRESULT) -> CameraFault {
    match hr.0 as u32 {
        // E_ACCESSDENIED: the Windows camera privacy switch (for desktop apps) is off.
        0x8007_0005 => CameraFault::AccessDenied,
        // MF_E_HW_MFT_FAILED_START_STREAMING, MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED,
        // ERROR_SHARING_VIOLATION, ERROR_BUSY, ERROR_DEVICE_IN_USE.
        0xC00D_3704 | 0xC00D_3EA3 | 0x8007_0020 | 0x8007_00AA | 0x8007_0964 => CameraFault::InUse,
        // MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED, ERROR_DEVICE_NOT_CONNECTED,
        // ERROR_GEN_FAILURE (USB device gone), ERROR_DEVICE_REMOVED, ERROR_FILE_NOT_FOUND.
        0xC00D_3EA2 | 0x8007_048F | 0x8007_001F | 0x8007_0651 | 0x8007_0002 => {
            CameraFault::Disconnected
        }
        // MF_E_INVALIDMEDIATYPE, MF_E_TOPO_CODEC_NOT_FOUND, MF_E_UNSUPPORTED_D3D_TYPE.
        0xC00D_36B4 | 0xC00D_5212 | 0xC00D_6D76 => CameraFault::Unsupported,
        _ => CameraFault::Other,
    }
}

fn mf_err(ctx: &str, e: windows_core::Error) -> CameraError {
    let hr = e.code();
    cam_err(
        fault_of(hr),
        format!("{ctx} failed: hr=0x{:08X} {}", hr.0 as u32, e.message()),
    )
}

fn hr_err(ctx: &str, hr: HRESULT) -> CameraError {
    mf_err(ctx, windows_core::Error::from(hr))
}

fn cap_err(e: CaptureError) -> CameraError {
    match e {
        CaptureError::Unsupported(m) => cam_err(CameraFault::Unsupported, m),
        CaptureError::NotFound(m) => cam_err(CameraFault::NotFound, m),
        other => cam_err(CameraFault::Other, other.to_string()),
    }
}

fn fake_camera_env() -> bool {
    std::env::var("VOX_FAKE_CAMERA").is_ok_and(|v| !v.is_empty() && v != "0")
}

// ---------------------------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------------------------

/// Run `f` on a fresh MTA thread (the caller's COM apartment stays untouched).
fn on_mta_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    std::thread::Builder::new()
        .name("vox-camera-enum".into())
        .spawn(move || {
            let _com = ComInit::mta();
            f()
        })
        .ok()?
        .join()
        .ok()
}

fn allocated_string(a: &IMFActivate, key: &GUID) -> Option<String> {
    unsafe {
        let mut p = PWSTR::null();
        let mut len = 0u32;
        a.GetAllocatedString(key, &mut p, &mut len).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// Every video capture device as (activation object, info). MTA thread, MF started.
fn enum_devices() -> std::result::Result<Vec<(IMFActivate, CameraInfo)>, CameraError> {
    enum_devices_in(None)
}

/// [`enum_devices`] through another device interface category (`KSCATEGORY_VIDEO_CAMERA` is
/// Windows' default).
fn enum_devices_in(category: Option<&GUID>) -> std::result::Result<Vec<(IMFActivate, CameraInfo)>, CameraError> {
    mf_startup().map_err(cap_err)?;
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 2).map_err(|e| mf_err("MFCreateAttributes", e))?;
        let attrs = attrs.ok_or_else(|| cam_err(CameraFault::Other, "MFCreateAttributes null"))?;
        attrs
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(|e| mf_err("SetGUID(SOURCE_TYPE)", e))?;
        if let Some(c) = category {
            attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_CATEGORY, c).map_err(|e| mf_err("SetGUID(VIDCAP_CATEGORY)", e))?;
        }
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(&attrs, &mut list, &mut count)
            .map_err(|e| mf_err("MFEnumDeviceSources", e))?;
        let mut out = Vec::new();
        if !list.is_null() {
            for slot in std::slice::from_raw_parts_mut(list, count as usize) {
                if let Some(a) = slot.take() {
                    let name = allocated_string(&a, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
                        .unwrap_or_else(|| "Câmera".into());
                    let id = allocated_string(&a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)
                        .unwrap_or_else(|| name.clone());
                    out.push((a, CameraInfo { id, name }));
                }
            }
            CoTaskMemFree(Some(list as *const _));
        }
        Ok(out)
    }
}

pub fn list_cameras() -> Result<Vec<CameraInfo>> {
    let devices = on_mta_thread(|| enum_devices().map(|v| v.into_iter().map(|(_, i)| i).collect::<Vec<_>>()))
        .ok_or_else(|| CaptureError::Os("camera enumeration thread failed".into()))?
        .map_err(CaptureError::from)?;
    let mut out = Vec::with_capacity(devices.len() + 1);
    if fake_camera_env() {
        out.push(synthetic_info());
    }
    out.extend(devices);
    Ok(out)
}

fn synthetic_info() -> CameraInfo {
    CameraInfo { id: SYNTHETIC_CAMERA_ID.into(), name: "Câmera de teste (sintética)".into() }
}

/// The modes the synthetic camera pretends to have.
fn synthetic_modes() -> Vec<CameraMode> {
    let m = |width: u32, height: u32, fps: u32, subtype| CameraMode { width, height, fps_milli: fps * 1000, subtype };
    let mut v = vec![
        m(1920, 1080, 15, CameraSubtype::Nv12),
        m(1280, 720, 30, CameraSubtype::Nv12),
        m(1280, 720, 15, CameraSubtype::Yuy2),
        m(960, 540, 30, CameraSubtype::Mjpg),
        m(640, 360, 30, CameraSubtype::Nv12),
    ];
    sort_modes(&mut v);
    v
}

/// One device open at a time in this process: two of our own opens overlapping (the formats
/// list while the preview starts, a preview closing as the call camera opens) make Windows hand
/// the second one the camera in shared mode, which lists only the format already streaming.
static DEVICE_LOCK: Mutex<()> = Mutex::new(());

fn device_lock() -> std::sync::MutexGuard<'static, ()> {
    DEVICE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Activate the device's media source. Call with [`device_lock`] held.
///
/// Normally a camera lists every native mode (a C920: dozens of MJPEG / YUY2 sizes and rates).
/// When Windows hands the camera out shared (another app holds it, or Windows shares it between
/// apps), it lists only the one format it streams, already converted (e.g. a single
/// "NV12 1920x1080 @ 30"): logged, since the format menu then has one entry.
unsafe fn activate_source(activate: &IMFActivate) -> std::result::Result<IMFMediaSource, CameraError> {
    let source: IMFMediaSource =
        activate.ActivateObject().map_err(|e| mf_err("IMFActivate::ActivateObject(camera)", e))?;
    if native_type_count(&source) == 1 {
        tracing::info!("camera lists a single native type (Windows is sharing or converting it): only that format can be chosen");
    }
    Ok(source)
}

/// Video media types of the source's first video stream (any subtype).
unsafe fn native_type_count(source: &IMFMediaSource) -> u32 {
    let Ok(pd) = source.CreatePresentationDescriptor() else { return 0 };
    for i in 0..pd.GetStreamDescriptorCount().unwrap_or(0) {
        let mut selected = windows_core::BOOL(0);
        let mut sd: Option<IMFStreamDescriptor> = None;
        if pd.GetStreamDescriptorByIndex(i, &mut selected, &mut sd).is_err() {
            continue;
        }
        let Some(handler) = sd.and_then(|sd| sd.GetMediaTypeHandler().ok()) else { continue };
        if handler.GetMajorType().ok() == Some(MFMediaType_Video) {
            return handler.GetMediaTypeCount().unwrap_or(0);
        }
    }
    0
}

/// "MJPG", "H264", "NV12"...: the FourCC of a Media Foundation video subtype, else the GUID.
fn subtype_text(g: &GUID) -> String {
    let b = g.data1.to_le_bytes();
    if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
        String::from_utf8_lossy(&b).trim().to_string()
    } else {
        format!("{g:?}")
    }
}

/// Native modes by the media source's presentation descriptor: no reader, no streaming.
unsafe fn source_modes(source: &IMFMediaSource) -> std::result::Result<Vec<Mode>, CameraError> {
    let pd = source
        .CreatePresentationDescriptor()
        .map_err(|e| mf_err("CreatePresentationDescriptor", e))?;
    let count = pd.GetStreamDescriptorCount().map_err(|e| mf_err("GetStreamDescriptorCount", e))?;
    let mut out = Vec::new();
    for i in 0..count {
        let mut selected = windows_core::BOOL(0);
        let mut sd: Option<IMFStreamDescriptor> = None;
        if pd.GetStreamDescriptorByIndex(i, &mut selected, &mut sd).is_err() {
            continue;
        }
        let Some(sd) = sd else { continue };
        let Ok(handler) = sd.GetMediaTypeHandler() else { continue };
        if handler.GetMajorType().ok() != Some(MFMediaType_Video) {
            continue;
        }
        let n = handler.GetMediaTypeCount().unwrap_or(0);
        for j in 0..n.min(512) {
            let Ok(t) = handler.GetMediaTypeByIndex(j) else { continue };
            tracing::trace!(index = j, subtype = ?t.GetGUID(&MF_MT_SUBTYPE).ok(), size = ?frame_size(&t), fps = frame_rate(&t), "camera media type");
            let Some(fmt) = t.GetGUID(&MF_MT_SUBTYPE).ok().and_then(|g| Fmt::from_subtype(&g)) else { continue };
            let Some((w, h)) = frame_size(&t) else { continue };
            if w < 32 || h < 32 {
                continue;
            }
            out.push(Mode { fmt, width: w, height: h, fps: frame_rate(&t) });
        }
    }
    Ok(out)
}

pub fn list_camera_formats(device: &str) -> std::result::Result<Vec<CameraMode>, CameraError> {
    if device == SYNTHETIC_CAMERA_ID || (device.is_empty() && fake_camera_env()) {
        return Ok(synthetic_modes());
    }
    let device = device.to_string();
    let res = on_mta_thread(move || {
        mf_startup().map_err(cap_err)?;
        let Device::Mf { activate, .. } = resolve(&device)? else { return Ok(synthetic_modes()) };
        let _lock = device_lock();
        let source = unsafe { activate_source(&activate) }?;
        let modes = unsafe { source_modes(&source) };
        unsafe {
            let _ = source.Shutdown();
            let _ = activate.ShutdownObject();
        }
        let mut v: Vec<CameraMode> = modes?.iter().map(to_camera_mode).collect();
        sort_modes(&mut v);
        Ok(v)
    })
    .unwrap_or_else(|| Err(cam_err(CameraFault::Other, "camera format thread failed")))?;
    Ok(res)
}

/// See [`crate::camera_diagnostics`].
pub fn camera_diagnostics(device: &str) -> std::result::Result<String, CameraError> {
    use std::fmt::Write;
    let device = device.to_string();
    on_mta_thread(move || {
        mf_startup().map_err(cap_err)?;
        let mut out = String::new();
        let _lock = device_lock();
        let (source, activate, device_id) = match resolve(&device)? {
            Device::Synthetic => {
                let _ = writeln!(out, "camera: synthetic");
                for m in synthetic_modes() {
                    let _ = writeln!(out, "  mode {}", m.label());
                }
                return Ok(out);
            }
            Device::Mf { activate, info } => {
                let _ = writeln!(out, "camera: {} ({})", info.name, info.id);
                (unsafe { activate_source(&activate) }?, activate, info.id)
            }
        };
        unsafe {
            if let Ok(pd) = source.CreatePresentationDescriptor() {
                for i in 0..pd.GetStreamDescriptorCount().unwrap_or(0) {
                    let mut selected = windows_core::BOOL(0);
                    let mut sd: Option<IMFStreamDescriptor> = None;
                    if pd.GetStreamDescriptorByIndex(i, &mut selected, &mut sd).is_err() {
                        continue;
                    }
                    let Some(handler) = sd.and_then(|sd| sd.GetMediaTypeHandler().ok()) else { continue };
                    let n = handler.GetMediaTypeCount().unwrap_or(0);
                    let _ = writeln!(out, "stream {i}: {n} native types (selected={})", selected.as_bool());
                    for j in 0..n.min(512) {
                        let Ok(t) = handler.GetMediaTypeByIndex(j) else { continue };
                        let sub = t.GetGUID(&MF_MT_SUBTYPE).map(|g| subtype_text(&g)).unwrap_or_default();
                        let (w, h) = frame_size(&t).unwrap_or((0, 0));
                        let _ = writeln!(out, "  {sub} {w}x{h} @ {:.2}", frame_rate(&t));
                    }
                }
            }
            // The same device through the other interface categories (diagnostics only).
            let key = super::dshow::device_instance_key(&device_id);
            let _ = source.Shutdown();
            let _ = activate.ShutdownObject();
            use windows::Win32::Media::KernelStreaming::{KSCATEGORY_CAPTURE, KSCATEGORY_VIDEO};
            for (label, cat) in [("KSCATEGORY_VIDEO", KSCATEGORY_VIDEO), ("KSCATEGORY_CAPTURE", KSCATEGORY_CAPTURE)] {
                match enum_devices_in(Some(&cat)) {
                    Ok(list) => match list.into_iter().find(|(_, i)| super::dshow::device_instance_key(&i.id) == key) {
                        Some((a, i)) => match a.ActivateObject::<IMFMediaSource>() {
                            Ok(src) => {
                                let modes = source_modes(&src).unwrap_or_default();
                                let _ = writeln!(out, "via {label} ({}): {} native types, {} usable", i.id, native_type_count(&src), modes.len());
                                let mut v: Vec<CameraMode> = modes.iter().map(to_camera_mode).collect();
                                sort_modes(&mut v);
                                for m in crate::camera_mode_menu(&v).iter().take(6) {
                                    let _ = writeln!(out, "  {}", m.label());
                                }
                                let _ = src.Shutdown();
                                let _ = a.ShutdownObject();
                            }
                            Err(e) => {
                                let _ = writeln!(out, "via {label}: activate failed: {e}");
                            }
                        },
                        None => {
                            let _ = writeln!(out, "via {label}: device not listed");
                        }
                    },
                    Err(e) => {
                        let _ = writeln!(out, "via {label}: enumeration failed: {e}");
                    }
                }
            }
            let source = activate_source(&activate)?;
            let ctl = Controls::query(&source);
            let _ = writeln!(out, "{} controls:", ctl.list().len());
            for c in ctl.list() {
                let _ = writeln!(
                    out,
                    "  {}[{}..{}/{} def {} now {}{}{}]",
                    c.id.key(), c.min, c.max, c.step, c.default, c.value,
                    if c.can_auto { " can-auto" } else { "" },
                    if c.auto { " auto" } else { "" },
                );
            }
            let _ = source.Shutdown();
            let _ = activate.ShutdownObject();
        }
        Ok(out)
    })
    .unwrap_or_else(|| Err(cam_err(CameraFault::Other, "camera diagnostics thread failed")))
}

enum Device {
    Synthetic,
    Mf { activate: IMFActivate, info: CameraInfo },
}

/// `device`: id, friendly name, or empty for the default.
fn resolve(device: &str) -> std::result::Result<Device, CameraError> {
    if device == SYNTHETIC_CAMERA_ID || (device.is_empty() && fake_camera_env()) {
        return Ok(Device::Synthetic);
    }
    let mut devices = enum_devices()?;
    if devices.is_empty() {
        return Err(cam_err(CameraFault::NotFound, "no video capture device"));
    }
    let i = if device.is_empty() {
        0
    } else {
        devices
            .iter()
            .position(|(_, i)| i.id == device)
            .or_else(|| devices.iter().position(|(_, i)| i.name.eq_ignore_ascii_case(device)))
            .ok_or_else(|| cam_err(CameraFault::NotFound, format!("no camera {device:?}")))?
    };
    let (activate, info) = devices.swap_remove(i);
    Ok(Device::Mf { activate, info })
}

// ---------------------------------------------------------------------------------------------
// Mode choice (pure, unit-tested)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fmt {
    Nv12,
    Yuy2,
    Mjpg,
}

impl Fmt {
    fn from_subtype(g: &GUID) -> Option<Fmt> {
        if *g == MFVideoFormat_NV12 {
            Some(Fmt::Nv12)
        } else if *g == MFVideoFormat_YUY2 {
            Some(Fmt::Yuy2)
        } else if *g == MFVideoFormat_MJPG {
            Some(Fmt::Mjpg)
        } else {
            None
        }
    }

    fn name(self) -> &'static str {
        match self {
            Fmt::Nv12 => "NV12",
            Fmt::Yuy2 => "YUY2",
            Fmt::Mjpg => "MJPG",
        }
    }

    fn rank(self) -> u32 {
        match self {
            Fmt::Nv12 => 0,
            Fmt::Yuy2 => 1,
            Fmt::Mjpg => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Mode {
    fmt: Fmt,
    width: u32,
    height: u32,
    fps: f32,
}

/// Lower is better: (misses the frame rate, size distance, too fast, format).
fn mode_cost(m: &Mode, want_w: u32, want_h: u32, want_fps: u32) -> (u32, u32, u32, u32) {
    let fps_short = if m.fps + 0.5 >= want_fps as f32 {
        0
    } else {
        // Among modes that are too slow, the fastest wins.
        1 + ((want_fps as f32 - m.fps).max(0.0) * 10.0) as u32
    };
    let size = if m.height >= want_h {
        m.height - want_h
    } else {
        10_000 + (want_h - m.height) * 4
    };
    let aspect = |w: u32, h: u32| w as f32 / h.max(1) as f32;
    let aspect_miss = if (aspect(m.width, m.height) - aspect(want_w, want_h)).abs() > 0.05 { 500 } else { 0 };
    let too_fast = (m.fps > want_fps as f32 * 1.5) as u32;
    (fps_short, size + aspect_miss, too_fast, m.fmt.rank())
}

fn pick_mode(modes: &[Mode], want_w: u32, want_h: u32, want_fps: u32) -> Option<usize> {
    (0..modes.len()).min_by_key(|&i| mode_cost(&modes[i], want_w, want_h, want_fps))
}

fn subtype_of(f: Fmt) -> CameraSubtype {
    match f {
        Fmt::Nv12 => CameraSubtype::Nv12,
        Fmt::Yuy2 => CameraSubtype::Yuy2,
        Fmt::Mjpg => CameraSubtype::Mjpg,
    }
}

fn to_camera_mode(m: &Mode) -> CameraMode {
    CameraMode { width: m.width, height: m.height, fps_milli: (m.fps * 1000.0).round() as u32, subtype: subtype_of(m.fmt) }
}

/// The native mode the user chose, if this camera still offers it.
fn find_mode(modes: &[Mode], want: &CameraMode) -> Option<usize> {
    modes.iter().position(|m| to_camera_mode(m).matches(want))
}

/// Biggest first, then fastest, uncompressed before MJPEG; exact duplicates dropped.
fn sort_modes(v: &mut Vec<CameraMode>) {
    crate::sort_camera_modes(v);
}

/// The transform as one atomic word: bit 0 mirror preview, bit 1 mirror others, bits 2-3 quarter
/// turns, bits 8.. zoom percent.
fn pack_transform(t: &CameraTransform) -> u64 {
    let rot = (t.rotation.degrees() / 90) as u64;
    t.mirror_preview as u64 | (t.mirror_others as u64) << 1 | rot << 2 | (t.zoom_pct as u64) << 8
}

fn unpack_transform(v: u64) -> CameraTransform {
    CameraTransform {
        mirror_preview: v & 1 != 0,
        mirror_others: v & 2 != 0,
        rotation: CameraRotation::from_degrees((((v >> 2) & 3) * 90) as u32),
        zoom_pct: ((v >> 8) as u32).clamp(100, CameraTransform::MAX_ZOOM_PCT),
    }
}

// ---------------------------------------------------------------------------------------------
// Source reader callback
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct SlotState {
    sample: Option<(IMFSample, i64)>,
    error: Option<HRESULT>,
    eos: bool,
    type_changed: bool,
    /// The read in flight was answered (with or without a sample: stream ticks have none).
    answered: bool,
    replaced: u64,
}

// SAFETY: MF samples are free-threaded; the state is only touched under the mutex.
unsafe impl Send for SlotState {}

struct SampleSlot {
    state: Mutex<SlotState>,
    /// Auto-reset: a read was answered.
    ev: Event,
    /// Manual-reset: `Flush` finished.
    flushed: Event,
}

impl SampleSlot {
    fn take(&self) -> SlotState {
        std::mem::take(&mut *self.state.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

#[implement(IMFSourceReaderCallback)]
struct ReaderCallback {
    slot: Arc<SampleSlot>,
}

impl IMFSourceReaderCallback_Impl for ReaderCallback_Impl {
    fn OnReadSample(
        &self,
        hrstatus: HRESULT,
        _stream: u32,
        flags: u32,
        _timestamp: i64,
        sample: windows_core::Ref<IMFSample>,
    ) -> windows_core::Result<()> {
        let now = monotonic_now_us();
        {
            let mut s = self.slot.state.lock().unwrap_or_else(|p| p.into_inner());
            s.answered = true;
            if hrstatus.is_err() {
                s.error = Some(hrstatus);
            } else if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                s.error = Some(E_FAIL);
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                s.eos = true;
            }
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                s.type_changed = true;
            }
            if let Ok(smp) = sample.ok() {
                if s.sample.replace((smp.clone(), now)).is_some() {
                    s.replaced += 1;
                }
            }
        }
        self.slot.ev.set();
        Ok(())
    }

    fn OnFlush(&self, _stream: u32) -> windows_core::Result<()> {
        self.slot.flushed.set();
        Ok(())
    }

    fn OnEvent(&self, _stream: u32, _event: windows_core::Ref<IMFMediaEvent>) -> windows_core::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Media Foundation camera
// ---------------------------------------------------------------------------------------------

/// The reader's current output.
#[derive(Clone, Copy, Debug)]
struct OutType {
    fmt: DXGI_FORMAT,
    width: u32,
    height: u32,
    space: YuvSpace,
}

struct MfCamera {
    activate: IMFActivate,
    source: IMFMediaSource,
    reader: Option<IMFSourceReader>,
    slot: Arc<SampleSlot>,
    _devmgr: IMFDXGIDeviceManager,
    native: Mode,
    /// Every usable native mode the device listed.
    modes: Vec<Mode>,
    out: OutType,
    name: String,
}

fn frame_size(t: &IMFMediaType) -> Option<(u32, u32)> {
    let v = unsafe { t.GetUINT64(&MF_MT_FRAME_SIZE) }.ok()?;
    Some(((v >> 32) as u32, v as u32))
}

fn frame_rate(t: &IMFMediaType) -> f32 {
    match unsafe { t.GetUINT64(&MF_MT_FRAME_RATE) } {
        Ok(v) if v as u32 != 0 => (v >> 32) as u32 as f32 / v as u32 as f32,
        _ => 0.0,
    }
}

impl MfCamera {
    fn open(
        activate: IMFActivate,
        info: &CameraInfo,
        device: &ID3D11Device,
        cfg: &CameraConfig,
    ) -> std::result::Result<Self, CameraError> {
        unsafe {
            let _lock = device_lock();
            let source = activate_source(&activate)?;
            match Self::open_reader(&activate, source.clone(), info, device, cfg) {
                Ok(c) => Ok(c),
                Err(e) => {
                    let _ = source.Shutdown();
                    let _ = activate.ShutdownObject();
                    Err(e)
                }
            }
        }
    }

    unsafe fn open_reader(
        activate: &IMFActivate,
        source: IMFMediaSource,
        info: &CameraInfo,
        device: &ID3D11Device,
        cfg: &CameraConfig,
    ) -> std::result::Result<Self, CameraError> {
        let mut token = 0u32;
        let mut mgr: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut token, &mut mgr).map_err(|e| mf_err("MFCreateDXGIDeviceManager", e))?;
        let mgr = mgr.ok_or_else(|| cam_err(CameraFault::Other, "device manager null"))?;
        mgr.ResetDevice(device, token).map_err(|e| mf_err("IMFDXGIDeviceManager::ResetDevice", e))?;

        let slot = Arc::new(SampleSlot {
            state: Mutex::new(SlotState::default()),
            ev: Event::new(false).map_err(cap_err)?,
            flushed: Event::new(true).map_err(cap_err)?,
        });
        let cb: IMFSourceReaderCallback = ReaderCallback { slot: slot.clone() }.into();
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 5).map_err(|e| mf_err("MFCreateAttributes", e))?;
        let attrs = attrs.ok_or_else(|| cam_err(CameraFault::Other, "MFCreateAttributes null"))?;
        let set = |r: windows_core::Result<()>, what: &str| r.map_err(|e| mf_err(what, e));
        set(attrs.SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &cb), "SetUnknown(ASYNC_CALLBACK)")?;
        set(attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &mgr), "SetUnknown(D3D_MANAGER)")?;
        set(attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1), "SetUINT32(HW_TRANSFORMS)")?;
        set(attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1), "SetUINT32(ADVANCED_VP)")?;
        set(attrs.SetUINT32(&MF_LOW_LATENCY, 1), "SetUINT32(LOW_LATENCY)")?;
        let reader = MFCreateSourceReaderFromMediaSource(&source, &attrs)
            .map_err(|e| mf_err("MFCreateSourceReaderFromMediaSource", e))?;
        let _ = reader.SetStreamSelection(ALL_STREAMS, false);
        reader
            .SetStreamSelection(FIRST_VIDEO, true)
            .map_err(|e| mf_err("SetStreamSelection(video)", e))?;

        // Native modes we can use.
        let mut modes = Vec::new();
        let mut types = Vec::new();
        for i in 0..512u32 {
            let Ok(t) = reader.GetNativeMediaType(FIRST_VIDEO, i) else { break };
            let Some(fmt) = t.GetGUID(&MF_MT_SUBTYPE).ok().and_then(|g| Fmt::from_subtype(&g)) else { continue };
            let Some((w, h)) = frame_size(&t) else { continue };
            if w < 32 || h < 32 {
                continue;
            }
            modes.push(Mode { fmt, width: w, height: h, fps: frame_rate(&t) });
            types.push(t);
        }
        let wanted = cfg.mode.and_then(|m| {
            let i = find_mode(&modes, &m);
            if i.is_none() {
                tracing::warn!(want = %m.label(), "the camera does not offer the chosen mode; picking one automatically");
            }
            i
        });
        let Some(pick) = wanted.or_else(|| pick_mode(&modes, cfg.width, cfg.height, cfg.fps)) else {
            return Err(cam_err(
                CameraFault::Unsupported,
                "the camera offers no NV12, YUY2 or MJPEG mode",
            ));
        };
        let native = modes[pick];
        tracing::info!(
            camera = %info.name,
            modes = modes.len(),
            chosen = format!("{} {}x{} @ {:.1}", native.fmt.name(), native.width, native.height, native.fps),
            chosen_by_user = wanted.is_some(),
            wanted = format!("{}x{} @ {}", cfg.width, cfg.height, cfg.fps),
            "camera mode chosen"
        );
        reader
            .SetCurrentMediaType(FIRST_VIDEO, None, &types[pick])
            .map_err(|e| mf_err("SetCurrentMediaType(native)", e))?;
        if native.fmt == Fmt::Mjpg {
            // Let Media Foundation decode (hardware MJPEG decoder when the GPU has one).
            let mut ok = false;
            let mut last = None;
            for sub in [MFVideoFormat_NV12, MFVideoFormat_YUY2] {
                let t = MFCreateMediaType().map_err(|e| mf_err("MFCreateMediaType", e))?;
                set(t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video), "SetGUID(MAJOR)")?;
                set(t.SetGUID(&MF_MT_SUBTYPE, &sub), "SetGUID(SUBTYPE)")?;
                set(
                    t.SetUINT64(&MF_MT_FRAME_SIZE, ((native.width as u64) << 32) | native.height as u64),
                    "SetUINT64(FRAME_SIZE)",
                )?;
                match reader.SetCurrentMediaType(FIRST_VIDEO, None, &t) {
                    Ok(()) => {
                        ok = true;
                        break;
                    }
                    Err(e) => last = Some(e),
                }
            }
            if !ok {
                return Err(match last {
                    Some(e) => mf_err("SetCurrentMediaType(MJPEG -> NV12/YUY2)", e),
                    None => cam_err(CameraFault::Unsupported, "no MJPEG decoder"),
                });
            }
        }
        let out = Self::read_out_type(&reader, native.fmt)?;
        Ok(MfCamera {
            activate: activate.clone(),
            source,
            reader: Some(reader),
            slot,
            _devmgr: mgr,
            native,
            modes,
            out,
            name: info.name.clone(),
        })
    }

    unsafe fn read_out_type(reader: &IMFSourceReader, native: Fmt) -> std::result::Result<OutType, CameraError> {
        let t = reader
            .GetCurrentMediaType(FIRST_VIDEO)
            .map_err(|e| mf_err("GetCurrentMediaType", e))?;
        let sub = t.GetGUID(&MF_MT_SUBTYPE).map_err(|e| mf_err("GetGUID(SUBTYPE)", e))?;
        let fmt = match Fmt::from_subtype(&sub) {
            Some(Fmt::Nv12) => DXGI_FORMAT_NV12,
            Some(Fmt::Yuy2) => DXGI_FORMAT_YUY2,
            _ => {
                return Err(cam_err(
                    CameraFault::Unsupported,
                    format!("unexpected camera output subtype {sub:?}"),
                ))
            }
        };
        let (width, height) = frame_size(&t).ok_or_else(|| cam_err(CameraFault::Unsupported, "no frame size"))?;
        // UVC cameras send BT.601 limited range unless they say otherwise; decoded JPEG is
        // BT.601 full range.
        let bt709 = t.GetUINT32(&MF_MT_YUV_MATRIX).ok() == Some(MFVideoTransferMatrix_BT709.0 as u32);
        let full_range = match t.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) {
            Ok(v) => v == MFNominalRange_0_255.0 as u32,
            Err(_) => native == Fmt::Mjpg,
        };
        Ok(OutType { fmt, width: width & !1, height: height & !1, space: YuvSpace { bt709, full_range } })
    }

    fn request(&self) -> std::result::Result<(), CameraError> {
        let Some(r) = &self.reader else { return Ok(()) };
        unsafe { r.ReadSample(FIRST_VIDEO, 0, None, None, None, None) }.map_err(|e| mf_err("ReadSample", e))
    }

    /// Flush, release the reader and shut the device down (camera light off).
    fn close(&mut self) {
        if let Some(r) = self.reader.take() {
            self.slot.flushed.reset();
            if unsafe { r.Flush(ALL_STREAMS) }.is_ok() {
                unsafe {
                    windows::Win32::System::Threading::WaitForSingleObject(self.slot.flushed.raw(), 1000);
                }
            }
            drop(r);
        }
        unsafe {
            let _ = self.source.Shutdown();
            let _ = self.activate.ShutdownObject();
        }
    }
}

impl Drop for MfCamera {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------------------------------------------------------------------------------------------
// System-memory frames -> GPU
// ---------------------------------------------------------------------------------------------

/// A DEFAULT texture fed through a small ring of staging textures (no Map ever waits on a copy
/// still in flight).
struct Uploader {
    fmt: DXGI_FORMAT,
    width: u32,
    height: u32,
    staging: Vec<ID3D11Texture2D>,
    next: usize,
    tex: ID3D11Texture2D,
}

fn texture(
    device: &ID3D11Device,
    fmt: DXGI_FORMAT,
    w: u32,
    h: u32,
    usage: D3D11_USAGE,
    bind: u32,
    cpu: u32,
) -> windows_core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: fmt,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut t = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut t))? };
    t.ok_or_else(windows_core::Error::empty)
}

impl Uploader {
    fn new(device: &ID3D11Device, fmt: DXGI_FORMAT, width: u32, height: u32) -> Result<Self> {
        let os = |e| super::os("CreateTexture2D(camera upload)", e);
        // Video-processor input views need a render-target (or decoder) bindable surface.
        let tex = texture(device, fmt, width, height, D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET.0 as u32, 0)
            .or_else(|_| texture(device, fmt, width, height, D3D11_USAGE_DEFAULT, D3D11_BIND_DECODER.0 as u32, 0))
            .or_else(|_| texture(device, fmt, width, height, D3D11_USAGE_DEFAULT, D3D11_BIND_SHADER_RESOURCE.0 as u32, 0))
            .map_err(os)?;
        let staging = (0..STAGING_RING)
            .map(|_| texture(device, fmt, width, height, D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_WRITE.0 as u32))
            .collect::<windows_core::Result<Vec<_>>>()
            .map_err(os)?;
        Ok(Self { fmt, width, height, staging, next: 0, tex })
    }

    fn matches(&self, fmt: DXGI_FORMAT, w: u32, h: u32) -> bool {
        self.fmt == fmt && self.width == w && self.height == h
    }

    /// `write(dst, pitch)` fills the mapped staging texture (NV12: the UV plane starts at
    /// `pitch * height`). Then the GPU copies it into the upload texture.
    fn upload(&mut self, ctx: &ID3D11DeviceContext, write: impl FnOnce(*mut u8, usize)) -> Result<&ID3D11Texture2D> {
        let st = &self.staging[self.next];
        self.next = (self.next + 1) % self.staging.len();
        unsafe {
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(st, 0, D3D11_MAP_WRITE, 0, Some(&mut m))
                .map_err(|e| super::os("Map(camera staging)", e))?;
            write(m.pData as *mut u8, m.RowPitch as usize);
            ctx.Unmap(st, 0);
            ctx.CopyResource(&self.tex, st);
        }
        Ok(&self.tex)
    }
}

/// Copy one frame from a (possibly padded) CPU buffer into a mapped staging texture.
///
/// # Safety
/// `src` holds the frame at `src_pitch` (NV12: UV plane at `uv_offset`); `dst` is a mapped
/// staging texture of the same format and size with row pitch `dst_pitch`.
#[allow(clippy::too_many_arguments)]
unsafe fn copy_frame(fmt: DXGI_FORMAT, src: *const u8, src_pitch: usize, uv_offset: usize, dst: *mut u8, dst_pitch: usize, w: usize, h: usize) {
    let rows = |s: *const u8, d: *mut u8, row: usize, n: usize| {
        for r in 0..n {
            std::ptr::copy_nonoverlapping(s.add(r * src_pitch), d.add(r * dst_pitch), row);
        }
    };
    if fmt == DXGI_FORMAT_YUY2 {
        rows(src, dst, w * 2, h);
    } else {
        rows(src, dst, w, h);
        rows(src.add(uv_offset), dst.add(dst_pitch * h), w, h / 2);
    }
}

// ---------------------------------------------------------------------------------------------
// Synthetic camera
// ---------------------------------------------------------------------------------------------

struct Synthetic {
    width: u32,
    height: u32,
    fps: u32,
    timer: OwnedHandle,
    index: u64,
    /// `VOX_FAKE_CAMERA_LOWLIGHT=1`: with automatic exposure on, deliver a third of the frames
    /// (what a real camera does in a dim room), so the low-light hint can be exercised.
    slow: bool,
    /// `VOX_FAKE_CAMERA_FORMAT=yuy2`: deliver packed YUY2 like a USB camera (exercises the
    /// YUY2 upload and conversion path); the picture is rendered as NV12 first.
    yuy2: Option<Vec<u8>>,
    /// `VOX_FAKE_CAMERA_NOISE`: the picture is rendered here first (see [`fake_noise`]).
    scratch: Vec<u8>,
}

impl Synthetic {
    fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        let timer = unsafe {
            CreateWaitableTimerExW(None, PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
                .or_else(|_| CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0))
                .map_err(|e| super::os("CreateWaitableTimerExW", e))?
        };
        Self::arm(timer, fps)?;
        let yuy2 = std::env::var("VOX_FAKE_CAMERA_FORMAT")
            .is_ok_and(|v| v.eq_ignore_ascii_case("yuy2"))
            .then(|| vec![0u8; width as usize * height as usize * 3 / 2]);
        Ok(Self { width, height, fps, timer: OwnedHandle(timer), index: 0, slow: false, yuy2, scratch: Vec::new() })
    }

    fn arm(timer: HANDLE, fps: u32) -> Result<()> {
        let period_ms = (1000 / fps.max(1)) as i32;
        let due: i64 = -(period_ms as i64 * 10_000);
        unsafe { SetWaitableTimer(timer, &due, period_ms, None, None, false) }
            .map_err(|e| super::os("SetWaitableTimer", e))
    }

    /// Dim-room simulation (see the field); a no-op without the environment variable.
    fn set_slow(&mut self, on: bool) {
        let want = on && std::env::var("VOX_FAKE_CAMERA_LOWLIGHT").is_ok_and(|v| !v.is_empty() && v != "0");
        if want != self.slow {
            self.slow = want;
            let fps = if want { (self.fps / 3).max(1) } else { self.fps };
            let _ = Self::arm(self.timer.0, fps);
            tracing::info!(slow = want, fps, "synthetic camera rate");
        }
    }

    fn format(&self) -> DXGI_FORMAT {
        if self.yuy2.is_some() {
            DXGI_FORMAT_YUY2
        } else {
            DXGI_FORMAT_NV12
        }
    }

    /// Render the next frame into a mapped staging texture of [`Synthetic::format`].
    ///
    /// # Safety
    /// `dst` is a mapped staging texture of that format and size with row pitch `pitch`.
    unsafe fn render_to(&mut self, dst: *mut u8, pitch: usize) {
        let noise = fake_noise();
        if noise > 0 && self.yuy2.is_none() {
            // Rendered in memory first: the mapped texture is write-combined (reads crawl).
            let (w, h) = (self.width as usize, self.height as usize);
            let mut buf = std::mem::take(&mut self.scratch);
            buf.resize(w * h * 3 / 2, 0);
            self.render(buf.as_mut_ptr(), w);
            add_noise(&mut buf[..w * h], noise, self.index);
            for r in 0..h * 3 / 2 {
                std::ptr::copy_nonoverlapping(buf.as_ptr().add(r * w), dst.add(r * pitch), w);
            }
            self.scratch = buf;
            return;
        }
        let Some(mut nv12) = self.yuy2.take() else {
            return self.render(dst, pitch);
        };
        let (w, h) = (self.width as usize, self.height as usize);
        self.render(nv12.as_mut_ptr(), w);
        if noise > 0 {
            add_noise(&mut nv12[..w * h], noise, self.index);
        }
        for r in 0..h {
            let y = &nv12[r * w..r * w + w];
            let uv = &nv12[w * h + (r / 2) * w..w * h + (r / 2) * w + w];
            let out = std::slice::from_raw_parts_mut(dst.add(r * pitch), w * 2);
            for x in (0..w).step_by(2) {
                out[x * 2] = y[x];
                out[x * 2 + 1] = uv[x];
                out[x * 2 + 2] = y[x + 1];
                out[x * 2 + 3] = uv[x + 1];
            }
        }
        self.yuy2 = Some(nv12);
    }

    /// A cheap camera-like picture: shaded background, a "head" moving side to side, a sweeping
    /// bar and a blinking corner box. Only row memsets.
    ///
    /// # Safety
    /// `dst` is a mapped NV12 staging texture of `width x height` with row pitch `pitch`.
    unsafe fn render(&mut self, dst: *mut u8, pitch: usize) {
        let (w, h) = (self.width as usize, self.height as usize);
        let i = self.index;
        self.index += 1;
        let row = |r: usize| std::slice::from_raw_parts_mut(dst.add(r * pitch), w);
        let uv_row = |r: usize| std::slice::from_raw_parts_mut(dst.add(pitch * h + r * pitch), w);
        for r in 0..h {
            row(r).fill((60 + r * 90 / h) as u8);
        }
        let t = i as f32 / self.fps.max(1) as f32;
        let (cx, cy, rad) = (
            (w as f32 * (0.5 + 0.25 * (t * 1.3).sin())) as isize,
            (h as f32 * 0.45) as isize,
            (h / 4) as isize,
        );
        let span = |dy: isize| ((rad * rad - dy * dy).max(0) as f32).sqrt() as isize;
        for r in (cy - rad).max(0)..(cy + rad).min(h as isize) {
            let s = span(r - cy);
            let (x0, x1) = ((cx - s).max(0) as usize, ((cx + s).min(w as isize)).max(0) as usize);
            if x1 > x0 {
                row(r as usize)[x0..x1].fill(185);
            }
        }
        let bar = (i as usize * 4) % h.saturating_sub(6).max(1);
        for r in bar..(bar + 6).min(h) {
            row(r).fill(230);
        }
        if (i / 15).is_multiple_of(2) {
            for r in 8..(8 + h / 12).min(h) {
                row(r)[8..(8 + w / 16).min(w)].fill(16);
            }
        }
        // Chroma: bluish background, skin-toned head.
        for r in 0..h / 2 {
            let u = uv_row(r);
            for px in u.as_chunks_mut::<2>().0 {
                px[0] = 140;
                px[1] = 120;
            }
            let dy = r as isize * 2 - cy;
            let s = span(dy);
            if dy.abs() < rad && s > 0 {
                let (x0, x1) = (((cx - s).max(0) as usize) & !1, ((cx + s).min(w as isize).max(0) as usize) & !1);
                for px in u[x0..x1.max(x0)].as_chunks_mut::<2>().0 {
                    px[0] = 108;
                    px[1] = 152;
                }
            }
        }
    }
}

/// `VOX_FAKE_CAMERA_NOISE=<n>` (v1.1 G5): luma noise of +-n levels on the synthetic camera, like a
/// real sensor's. The flat picture alone compresses to ~1/3 of a camera's bitrate, which no real
/// webcam does, and that skews the bandwidth estimate of a camera + Go Live test. 0 = none.
fn fake_noise() -> u8 {
    static N: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var("VOX_FAKE_CAMERA_NOISE").ok().and_then(|v| v.parse().ok()).unwrap_or(0).min(64))
}

/// Add +-`n` of pseudo-random noise to a luma plane (xorshift; new noise every frame).
fn add_noise(y: &mut [u8], n: u8, frame: u64) {
    let span = 2 * n as u64 + 1;
    let mut s = 0x9E37_79B9_7F4A_7C15u64 ^ frame.wrapping_mul(0xD1B5_4A32_D192_ED03);
    for chunk in y.chunks_mut(4) {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        for (k, p) in chunk.iter_mut().enumerate() {
            let d = ((s >> (k * 16)) & 0xFFFF) % span;
            *p = (*p as i32 + d as i32 - n as i32).clamp(16, 235) as u8;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Capture thread
// ---------------------------------------------------------------------------------------------

struct CamShared {
    captured: AtomicU64,
    /// Samples the device delivered that were replaced before the loop read them (a busy PC):
    /// they still count as the camera's frame rate.
    replaced: AtomicU64,
    preview_on: AtomicBool,
    preview: Arc<Shared>,
    failure: Mutex<Option<CameraError>>,
    format: Mutex<Option<CameraFormat>>,
    backend: Mutex<Option<EncoderBackend>>,
    /// Supported controls with their latest values (written by the camera thread).
    controls: Mutex<Vec<CameraControl>>,
    /// Control requests from other threads, and the event that wakes the camera thread for them.
    ctl_queue: Mutex<CtlQueue>,
    ctl_ev: Event,
    /// See `pack_transform`.
    transform: AtomicU64,
    /// The video processor can mirror / rotate (cleared by the first frame on a GPU that cannot).
    transform_ok: AtomicBool,
    /// Frames per second over the last window, times 1000.
    fps_milli: AtomicU32,
    low_light: AtomicBool,
    /// Automatic exposure is on and allowed to lower the frame rate (false when the camera has
    /// no exposure control, or its auto-exposure priority keeps the frame rate).
    exposure_auto: AtomicBool,
    /// Every native mode of the opened device.
    modes: Mutex<Vec<CameraMode>>,
}

impl CamShared {
    fn new(preview_on: bool, transform: CameraTransform) -> std::result::Result<Self, CameraError> {
        Ok(Self {
            captured: AtomicU64::new(0),
            replaced: AtomicU64::new(0),
            preview_on: AtomicBool::new(preview_on),
            preview: Arc::default(),
            failure: Mutex::new(None),
            format: Mutex::new(None),
            backend: Mutex::new(None),
            controls: Mutex::new(Vec::new()),
            ctl_queue: Mutex::new(CtlQueue::default()),
            ctl_ev: Event::new(false).map_err(cap_err)?,
            transform: AtomicU64::new(pack_transform(&transform)),
            transform_ok: AtomicBool::new(true),
            fps_milli: AtomicU32::new(0),
            low_light: AtomicBool::new(false),
            exposure_auto: AtomicBool::new(false),
            modes: Mutex::new(Vec::new()),
        })
    }
}

enum Source {
    Mf(MfCamera),
    Synthetic(Synthetic),
}

impl Source {
    fn event(&self) -> HANDLE {
        match self {
            Source::Mf(c) => c.slot.ev.raw(),
            Source::Synthetic(s) => s.timer.0,
        }
    }
}

struct CameraLoop {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    /// `None` only during teardown (released first: the camera light goes off at once).
    source: Option<Source>,
    upload: Option<Uploader>,
    conv: Converter,
    pconv: Converter,
    main: Option<Box<dyn Sink>>,
    preview: Option<Readback>,
    pacer: Pacer,
    ppacer: Pacer,
    cfg: CameraConfig,
    out_size: (u32, u32),
    preview_size: (u32, u32),
    shared: Arc<Shared>,
    cs: Arc<CamShared>,
    last_frame: Instant,
    consecutive_errors: u32,
    stats_gate: vox_log::RateLimiter,
    last_stats: (Instant, u64, u64, u64),
    gpu_frames: bool,
    ctl: Controls,
    /// (start of the window, frames captured then): the effective frame rate once a second.
    fps_win: (Instant, u64),
    lowlight: LowLightDetector,
    /// The rate the opened mode promises.
    requested_fps: f32,
}

pub struct CameraInner {
    stop: Arc<Event>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    es: Arc<EncShared>,
    control: EncoderControl,
    cs: Arc<CamShared>,
}

impl CameraInner {
    pub fn control(&self) -> EncoderControl {
        self.control.clone()
    }

    pub fn backend(&self) -> Option<EncoderBackend> {
        *self.cs.backend.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn format(&self) -> CameraFormat {
        self.cs.format.lock().unwrap_or_else(|p| p.into_inner()).clone().unwrap_or(CameraFormat {
            device: String::new(),
            native_width: 0,
            native_height: 0,
            native_fps: 0.0,
            subtype: String::new(),
            gpu_frames: false,
            width: 0,
            height: 0,
        })
    }

    pub fn stats(&self) -> CameraStats {
        CameraStats {
            frames_captured: self.cs.captured.load(Ordering::Relaxed),
            preview_frames: self.cs.preview.delivered.load(Ordering::Relaxed),
            encoded: encode::encoded_stats(&self.shared, &self.es),
            fps: self.cs.fps_milli.load(Ordering::Relaxed) as f32 / 1000.0,
            low_light: self.cs.low_light.load(Ordering::Relaxed),
        }
    }

    pub fn controls(&self) -> Vec<CameraControl> {
        self.cs.controls.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn set_controls(&self, adjust: &[CameraAdjust]) {
        if adjust.is_empty() {
            return;
        }
        {
            let mut q = self.cs.ctl_queue.lock().unwrap_or_else(|p| p.into_inner());
            for a in adjust {
                q.push_set(*a);
            }
        }
        self.cs.ctl_ev.set();
    }

    pub fn reset_controls(&self, id: Option<CameraControlId>) {
        self.cs.ctl_queue.lock().unwrap_or_else(|p| p.into_inner()).push_reset(id);
        self.cs.ctl_ev.set();
    }

    pub fn refresh_controls(&self) {
        self.cs.ctl_queue.lock().unwrap_or_else(|p| p.into_inner()).push_refresh();
        self.cs.ctl_ev.set();
    }

    pub fn native_modes(&self) -> Vec<CameraMode> {
        self.cs.modes.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn set_transform(&self, t: CameraTransform) {
        let v = pack_transform(&t);
        if self.cs.transform.swap(v, Ordering::Relaxed) != v {
            tracing::debug!(?t, "camera transform changed");
        }
    }

    pub fn transform(&self) -> CameraTransform {
        unpack_transform(self.cs.transform.load(Ordering::Relaxed))
    }

    pub fn transform_supported(&self) -> bool {
        self.cs.transform_ok.load(Ordering::Relaxed)
    }

    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Relaxed)
    }

    pub fn failure(&self) -> Option<CameraError> {
        self.cs.failure.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn set_preview(&self, on: bool) {
        if self.cs.preview_on.swap(on, Ordering::Relaxed) != on {
            tracing::debug!(on, "camera preview toggled");
        }
    }
}

impl Drop for CameraInner {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The adapter that drives the primary monitor (where the hardware encoder lives).
fn primary_adapter() -> Option<windows::Win32::Graphics::Dxgi::IDXGIAdapter> {
    let hmon = super::enumerate::monitors_with_handles().ok().and_then(|m| {
        let primary = m.iter().position(|(_, i)| i.primary).unwrap_or(0);
        m.into_iter().nth(primary).map(|(h, _)| h)
    });
    hmon.and_then(super::enumerate::adapter_for_monitor)
}

pub fn start_camera(
    device: &str,
    cfg: CameraConfig,
    output: CameraOutput,
    on_preview: Option<FrameCallback>,
) -> std::result::Result<CameraInner, CameraError> {
    let cfg = CameraConfig {
        width: cfg.width.clamp(64, 3840) & !1,
        height: cfg.height.clamp(64, 2160) & !1,
        fps: cfg.fps.clamp(1, 60),
        transform: cfg.transform.sanitized(),
        preview_max_width: cfg.preview_max_width.clamp(16, 1920) & !1,
        preview_max_height: cfg.preview_max_height.clamp(16, 1080) & !1,
        ..cfg
    };
    let stop = Arc::new(Event::new(true).map_err(cap_err)?);
    let shared = Shared::new_running();
    let es = Arc::new(EncShared::default());
    let control = EncoderControl::default();
    let cs = Arc::new(CamShared::new(on_preview.is_some(), cfg.transform)?);
    let (tx, rx) = mpsc::sync_channel::<std::result::Result<(), CameraError>>(1);
    let device = device.to_string();
    let thread = {
        let (stop, shared, es, control, cs) = (stop.clone(), shared.clone(), es.clone(), control.clone(), cs.clone());
        std::thread::Builder::new()
            .name("vox-camera".into())
            .spawn(move || {
                let _com = ComInit::mta();
                let opened = (|| {
                    mf_startup().map_err(cap_err)?;
                    let dev = resolve(&device)?;
                    CameraLoop::open(dev, cfg, output, on_preview, shared.clone(), es, control, cs.clone(), &stop)
                })();
                match opened {
                    Ok(mut l) => {
                        let _ = tx.send(Ok(()));
                        let failure = l.run(&stop);
                        if let Some(f) = failure {
                            *cs.failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(f);
                        }
                        drop(l);
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
                shared.running.store(false, Ordering::Relaxed);
            })
            .map_err(|e| cam_err(CameraFault::Other, format!("spawn camera thread: {e}")))?
    };
    match rx.recv() {
        Ok(Ok(())) => Ok(CameraInner { stop, thread: Some(thread), shared, es, control, cs }),
        Ok(Err(e)) => {
            let _ = thread.join();
            tracing::warn!(fault = ?e.fault, detail = %e.detail, "camera start failed");
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(cam_err(CameraFault::Other, "camera thread exited during setup"))
        }
    }
}

impl CameraLoop {
    #[allow(clippy::too_many_arguments)]
    fn open(
        dev: Device,
        cfg: CameraConfig,
        output: CameraOutput,
        on_preview: Option<FrameCallback>,
        shared: Arc<Shared>,
        es: Arc<EncShared>,
        control: EncoderControl,
        cs: Arc<CamShared>,
        stop: &Event,
    ) -> std::result::Result<Self, CameraError> {
        let t0 = Instant::now();
        // The chosen encoder's adapter (v0.7), else the primary monitor's (the camera frames are
        // uploaded / decoded on whichever GPU this device is on).
        let adapter = match cfg.adapter_luid {
            Some(l) => Some(super::adapters::adapter_by_luid(l).ok_or_else(|| {
                cam_err(CameraFault::Unsupported, format!("adapter {l:#x} not found"))
            })?),
            None => primary_adapter(),
        };
        let (device, ctx, adapter_info) = video::create_device(adapter.as_ref()).map_err(cap_err)?;
        if cfg.adapter_luid.is_some_and(|l| l != super::adapters::luid_u64(adapter_info.luid)) {
            return Err(cam_err(
                CameraFault::Unsupported,
                format!("no D3D11 device on the encoder's adapter (got '{}')", adapter_info.name),
            ));
        }
        unsafe {
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                // Media Foundation uses the device from its own threads.
                let _ = mt.SetMultithreadProtected(true);
            }
        }
        let mut native_modes = synthetic_modes();
        let (source, native, out_fmt, out_wh, space, name) = match dev {
            Device::Synthetic => {
                // A chosen fake mode sets the size and rate; otherwise 720p scaled to fit.
                let (w, h, fps) = match cfg.mode {
                    Some(m) => (m.width & !1, m.height & !1, ((m.fps() + 0.5) as u32).clamp(1, 60)),
                    None => {
                        let (w, h) = video::fit(1280, 720, cfg.width, cfg.height);
                        (w, h, cfg.fps)
                    }
                };
                let s = Synthetic::new(w, h, fps).map_err(cap_err)?;
                let fmt = s.format();
                let native = Mode { fmt: if fmt == DXGI_FORMAT_YUY2 { Fmt::Yuy2 } else { Fmt::Nv12 }, width: w, height: h, fps: fps as f32 };
                let space = YuvSpace { bt709: false, full_range: false };
                (Source::Synthetic(s), native, fmt, (w, h), space, "synthetic".to_string())
            }
            Device::Mf { activate, info } => {
                let c = MfCamera::open(activate, &info, &device, &cfg)?;
                let (native, out, name) = (c.native, c.out, c.name.clone());
                native_modes = c.modes.iter().map(to_camera_mode).collect();
                sort_modes(&mut native_modes);
                (Source::Mf(c), native, out.fmt, (out.width, out.height), out.space, name)
            }
        };
        let out_size = camera_geometry(out_wh.0, out_wh.1, &cfg.transform, cfg.width, cfg.height).out;
        let preview_size =
            camera_geometry(out_wh.0, out_wh.1, &cfg.transform, cfg.preview_max_width, cfg.preview_max_height).out;
        // A quarter turn swaps width and height: the encoder is sized for either orientation.
        let enc_side = cfg.width.max(cfg.height) & !1;

        let main: Option<Box<dyn Sink>> = match output {
            CameraOutput::Encoded(cb) => {
                let p = EncParams {
                    width: out_size.0,
                    height: out_size.1,
                    fps: cfg.fps,
                    bitrate: cfg.bitrate_bps.clamp(100_000, 50_000_000),
                    max_width: out_size.0.max(enc_side),
                    max_height: out_size.1.max(enc_side),
                    tuning: Tuning::default(),
                };
                let ev = Arc::new(Event::new(true).map_err(cap_err)?);
                let sc = SinkCtx {
                    device: &device,
                    ctx: &ctx,
                    adapter_luid: adapter_info.luid,
                    adapter_vendor: adapter_info.vendor,
                    adapter_name: &adapter_info.name,
                    out_size,
                    cfg: crate::VideoConfig {
                        max_fps: cfg.fps,
                        max_width: enc_side,
                        max_height: enc_side,
                        capture_cursor: false,
                    },
                };
                let enc = encode::create_encoder(cfg.backend, &sc, p, &ev).map_err(|e| match e {
                    CaptureError::Unsupported(m) => cam_err(CameraFault::Unsupported, format!("no hardware encoder: {m}")),
                    other => cam_err(CameraFault::Unsupported, format!("hardware encoder failed: {other}")),
                })?;
                *cs.backend.lock().unwrap_or_else(|p| p.into_inner()) = Some(enc.backend());
                // A webcam keeps its frame rate under congestion (softer frames instead).
                Some(Box::new(EncoderSink::new(enc, ev, control, cb, es, p, DegradationPreference::Smoothness)))
            }
            CameraOutput::Raw(cb) => Some(Box::new(Readback::new(&device, &ctx, cb).map_err(cap_err)?)),
            CameraOutput::None => None,
        };
        let preview = match on_preview {
            Some(cb) => Some(Readback::new(&device, &ctx, cb).map_err(cap_err)?),
            None => None,
        };
        let mut conv = Converter::new(&device, &ctx).map_err(cap_err)?;
        let mut pconv = Converter::new(&device, &ctx).map_err(cap_err)?;
        conv.set_input_yuv(space);
        pconv.set_input_yuv(space);

        *cs.modes.lock().unwrap_or_else(|p| p.into_inner()) = native_modes;
        let l = CameraLoop {
            device,
            ctx,
            source: Some(source),
            upload: None,
            conv,
            pconv,
            main,
            preview,
            pacer: Pacer::new(cfg.fps),
            ppacer: Pacer::new(cfg.fps.min(30)),
            cfg,
            out_size,
            preview_size,
            shared,
            cs,
            last_frame: Instant::now(),
            consecutive_errors: 0,
            stats_gate: vox_log::RateLimiter::new(),
            last_stats: (Instant::now(), 0, 0, 0),
            gpu_frames: false,
            ctl: Controls::none(),
            fps_win: (Instant::now(), 0),
            lowlight: LowLightDetector::new(),
            requested_fps: native.fps,
        };

        // Start streaming and wait for the first answer, so "in use", "blocked" and "no
        // picture" come back from the start call instead of a silent tile.
        if let Some(Source::Mf(c)) = &l.source {
            c.request()?;
            let handles = [stop.raw(), c.slot.ev.raw()];
            let r = unsafe { WaitForMultipleObjects(&handles, false, FIRST_FRAME_TIMEOUT.as_millis() as u32) };
            if r == WAIT_OBJECT_0 {
                return Err(cam_err(CameraFault::Other, "stopped while starting"));
            }
            if r == WAIT_TIMEOUT {
                return Err(cam_err(
                    CameraFault::NoFrames,
                    format!("no frame within {} s", FIRST_FRAME_TIMEOUT.as_secs()),
                ));
            }
            {
                let st = c.slot.state.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(hr) = st.error {
                    return Err(hr_err("ReadSample (first frame)", hr));
                }
                if st.eos {
                    return Err(cam_err(CameraFault::Disconnected, "end of stream at start"));
                }
            }
            // Leave the answer for the loop.
            c.slot.ev.set();
        }
        let mut l = l;
        let ctl_t0 = Instant::now();
        l.ctl = match &l.source {
            Some(Source::Mf(c)) => Controls::query(&c.source),
            Some(Source::Synthetic(_)) => Controls::fake(),
            None => Controls::none(),
        };
        l.publish_controls();
        {
            let list: Vec<String> = l
                .ctl
                .list()
                .iter()
                .map(|c| format!("{}[{}..{}/{} def {}{}]", c.id.key(), c.min, c.max, c.step, c.default, if c.can_auto { " auto" } else { "" }))
                .collect();
            tracing::info!(controls = %list.join(" "), query_ms = ctl_t0.elapsed().as_millis() as u64, "camera controls");
        }
        let format = CameraFormat {
            device: name.clone(),
            native_width: native.width,
            native_height: native.height,
            native_fps: native.fps,
            subtype: if matches!(l.source, Some(Source::Synthetic(_))) { "synthetic".into() } else { native.fmt.name().into() },
            gpu_frames: false,
            width: out_size.0,
            height: out_size.1,
        };
        *l.cs.format.lock().unwrap_or_else(|p| p.into_inner()) = Some(format);
        tracing::info!(
            camera = %name,
            native = format!("{} {}x{} @ {:.1}", native.fmt.name(), native.width, native.height, native.fps),
            reader_out = format!("{} {}x{}", if out_fmt == DXGI_FORMAT_YUY2 { "YUY2" } else { "NV12" }, out_wh.0, out_wh.1),
            colour = format!("{} {}", if space.bt709 { "BT.709" } else { "BT.601" }, if space.full_range { "full" } else { "limited" }),
            out = format!("{}x{} @ {}", l.out_size.0, l.out_size.1, l.cfg.fps),
            preview = format!("{}x{}", l.preview_size.0, l.preview_size.1),
            sink = %l.main.as_ref().map(|s| s.describe()).unwrap_or_else(|| "preview only".into()),
            adapter = %adapter_info.name,
            open_ms = t0.elapsed().as_millis() as u64,
            "camera capture started"
        );
        Ok(l)
    }

    fn run(&mut self, stop: &Event) -> Option<CameraError> {
        let mut handles = vec![stop.raw(), self.source.as_ref().expect("source").event()];
        let main_idx = self.main.as_ref().map(|m| {
            handles.push(m.event());
            handles.len() - 1
        });
        let prev_idx = self.preview.as_ref().map(|p| {
            handles.push(Sink::event(p));
            handles.len() - 1
        });
        handles.push(self.cs.ctl_ev.raw());
        let ctl_idx = handles.len() - 1;
        let _ = self.stats_gate.ready(vox_log::STATS_INTERVAL);
        self.last_frame = Instant::now();
        let failure = loop {
            let r = unsafe { WaitForMultipleObjects(&handles, false, 1000) };
            let res = if r == WAIT_TIMEOUT {
                Ok(())
            } else {
                match (r.0.wrapping_sub(WAIT_OBJECT_0.0)) as usize {
                    0 => break None,
                    1 => self.on_source(),
                    i if Some(i) == main_idx => self.drain_main(),
                    i if Some(i) == prev_idx => self.drain_preview(),
                    i if i == ctl_idx => self.on_controls(),
                    _ => Err(cam_err(CameraFault::Other, "WaitForMultipleObjects failed")),
                }
            };
            if let Err(e) = res {
                if let Some(fatal) = self.handle_error(e) {
                    break Some(fatal);
                }
            }
            if self.last_frame.elapsed() > STALL_TIMEOUT {
                break Some(cam_err(
                    CameraFault::NoFrames,
                    format!("no frame for {} s", STALL_TIMEOUT.as_secs()),
                ));
            }
            self.maybe_log_stats();
            self.tick_fps();
        };
        tracing::info!(
            reason = %failure.as_ref().map(|f| f.to_string()).unwrap_or_else(|| "stopped by owner".into()),
            captured = self.cs.captured.load(Ordering::Relaxed),
            delivered = self.shared.delivered.load(Ordering::Relaxed),
            dropped = self.shared.dropped.load(Ordering::Relaxed),
            "camera capture stopped"
        );
        failure
    }

    /// Copy the controls' state where other threads can read it; keep the synthetic camera's
    /// dim-room simulation in step with its exposure mode.
    fn publish_controls(&mut self) {
        *self.cs.controls.lock().unwrap_or_else(|p| p.into_inner()) = self.ctl.list().to_vec();
        let slows = self.ctl.may_slow_down();
        self.cs.exposure_auto.store(slows.unwrap_or(false), Ordering::Relaxed);
        if let Some(Source::Synthetic(s)) = self.source.as_mut() {
            s.set_slow(slows == Some(true));
        }
    }

    /// Apply queued control changes (only when someone asked: never per frame).
    fn on_controls(&mut self) -> std::result::Result<(), CameraError> {
        let ops = self.cs.ctl_queue.lock().unwrap_or_else(|p| p.into_inner()).take();
        if !ops.is_empty() {
            self.ctl.apply(&ops);
            self.publish_controls();
        }
        Ok(())
    }

    /// Once a second: the frame rate the device really delivers, and the low-light verdict.
    fn tick_fps(&mut self) {
        let el = self.fps_win.0.elapsed();
        if el < Duration::from_secs(1) {
            return;
        }
        // What the device delivered, whether or not this thread got to every frame in time.
        let c = self.cs.captured.load(Ordering::Relaxed) + self.cs.replaced.load(Ordering::Relaxed);
        let secs = el.as_secs_f32();
        let fps = (c - self.fps_win.1) as f32 / secs;
        self.fps_win = (Instant::now(), c);
        self.cs.fps_milli.store((fps * 1000.0) as u32, Ordering::Relaxed);
        let auto = self.cs.exposure_auto.load(Ordering::Relaxed);
        let was = self.cs.low_light.load(Ordering::Relaxed);
        let low = self.lowlight.update(secs, fps, self.requested_fps, auto);
        if low != was {
            tracing::info!(low_light = low, fps = format!("{fps:.1}"), requested = self.requested_fps, "camera low-light state");
        }
        self.cs.low_light.store(low, Ordering::Relaxed);
    }

    fn handle_error(&mut self, e: CameraError) -> Option<CameraError> {
        if e.fault != CameraFault::Other || unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            return Some(e);
        }
        self.shared.drop_frames(1);
        self.consecutive_errors += 1;
        if self.consecutive_errors == 1 || self.consecutive_errors.is_multiple_of(10) {
            tracing::warn!(error = %e, count = self.consecutive_errors, "camera frame failed");
        }
        (self.consecutive_errors >= MAX_CONSECUTIVE_ERRORS).then_some(e)
    }

    fn drain_main(&mut self) -> std::result::Result<(), CameraError> {
        if let Some(m) = self.main.as_mut() {
            m.drain(&self.ctx, &self.shared).map_err(sink_err)?;
        }
        Ok(())
    }

    fn drain_preview(&mut self) -> std::result::Result<(), CameraError> {
        if let Some(p) = self.preview.as_mut() {
            Sink::drain(p, &self.ctx, &self.cs.preview).map_err(sink_err)?;
        }
        Ok(())
    }

    /// The source signalled: a sample (or error) from the reader, or the synthetic timer.
    fn on_source(&mut self) -> std::result::Result<(), CameraError> {
        match self.source.as_mut().expect("source") {
            Source::Synthetic(s) => {
                let (w, h, fmt) = (s.width, s.height, s.format());
                if !self.upload.as_ref().is_some_and(|u| u.matches(fmt, w, h)) {
                    self.upload = Some(Uploader::new(&self.device, fmt, w, h).map_err(cap_err)?);
                }
                let up = self.upload.as_mut().expect("made above");
                let tex = up.upload(&self.ctx, |dst, pitch| unsafe { s.render_to(dst, pitch) }).map_err(cap_err)?.clone();
                self.frame(&tex, 0, w, h, monotonic_now_us())
            }
            Source::Mf(c) => {
                let st = c.slot.take();
                if !st.answered {
                    return Ok(());
                }
                if let Some(hr) = st.error {
                    return Err(hr_err("ReadSample", hr));
                }
                if st.eos {
                    return Err(cam_err(CameraFault::Disconnected, "camera stream ended (unplugged?)"));
                }
                if st.type_changed {
                    let reader = c.reader.clone().expect("reader while running");
                    c.out = unsafe { MfCamera::read_out_type(&reader, c.native.fmt) }?;
                    self.conv.set_input_yuv(c.out.space);
                    self.pconv.set_input_yuv(c.out.space);
                    tracing::info!(w = c.out.width, h = c.out.height, "camera output type changed");
                }
                if st.replaced > 0 {
                    self.shared.drop_frames(st.replaced);
                    self.cs.replaced.fetch_add(st.replaced, Ordering::Relaxed);
                }
                // Keep exactly one read in flight.
                c.request()?;
                let Some((sample, ts)) = st.sample else { return Ok(()) };
                let out = c.out;
                self.mf_sample(&sample, out, ts)
            }
        }
    }

    fn mf_sample(&mut self, sample: &IMFSample, out: OutType, ts: i64) -> std::result::Result<(), CameraError> {
        let buf = unsafe { sample.GetBufferByIndex(0) }.map_err(|e| mf_err("GetBufferByIndex", e))?;
        if let Ok(dx) = buf.cast::<IMFDXGIBuffer>() {
            let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
            unsafe { dx.GetResource(&ID3D11Texture2D::IID, &mut p) }.map_err(|e| mf_err("IMFDXGIBuffer::GetResource", e))?;
            // SAFETY: GetResource returned an AddRef'ed ID3D11Texture2D.
            let tex = unsafe { ID3D11Texture2D::from_raw(p) };
            let slice = unsafe { dx.GetSubresourceIndex() }.unwrap_or(0);
            if !self.gpu_frames {
                self.gpu_frames = true;
                if let Some(f) = self.cs.format.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
                    f.gpu_frames = true;
                }
                tracing::info!(slice, "camera frames arrive as GPU textures");
            }
            return self.frame(&tex, slice, out.width, out.height, ts);
        }
        // System memory: one staging upload.
        if !self.upload.as_ref().is_some_and(|u| u.matches(out.fmt, out.width, out.height)) {
            self.upload = Some(Uploader::new(&self.device, out.fmt, out.width, out.height).map_err(cap_err)?);
        }
        let (w, h) = (out.width as usize, out.height as usize);
        let row_bytes = if out.fmt == DXGI_FORMAT_YUY2 { w * 2 } else { w };
        let up = self.upload.as_mut().expect("made above");
        let tex = unsafe {
            if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                let mut p: *mut u8 = std::ptr::null_mut();
                let mut pitch = 0i32;
                b2.Lock2D(&mut p, &mut pitch).map_err(|e| mf_err("IMF2DBuffer::Lock2D", e))?;
                let pitch = pitch.unsigned_abs() as usize;
                let len = buf.GetCurrentLength().unwrap_or(0) as usize;
                let uv = nv12_uv_offset(pitch, h, len);
                let r = if pitch < row_bytes {
                    Err(cam_err(CameraFault::Other, "camera buffer pitch too small"))
                } else {
                    up.upload(&self.ctx, |dst, dpitch| copy_frame(out.fmt, p, pitch, uv, dst, dpitch, w, h))
                        .cloned()
                        .map_err(cap_err)
                };
                let _ = b2.Unlock2D();
                r?
            } else {
                let mut p: *mut u8 = std::ptr::null_mut();
                let mut len = 0u32;
                buf.Lock(&mut p, None, Some(&mut len)).map_err(|e| mf_err("IMFMediaBuffer::Lock", e))?;
                let need = if out.fmt == DXGI_FORMAT_YUY2 { row_bytes * h } else { w * h * 3 / 2 };
                let r = if (len as usize) < need {
                    Err(cam_err(CameraFault::Other, format!("camera buffer too small ({len} < {need})")))
                } else {
                    up.upload(&self.ctx, |dst, dpitch| copy_frame(out.fmt, p, row_bytes, w * h, dst, dpitch, w, h))
                        .cloned()
                        .map_err(cap_err)
                };
                let _ = buf.Unlock();
                r?
            }
        };
        self.frame(&tex, 0, out.width, out.height, ts)
    }

    /// Convert and hand one camera frame to the sinks (each only if it can take one now).
    fn frame(&mut self, tex: &ID3D11Texture2D, slice: u32, w: u32, h: u32, ts: i64) -> std::result::Result<(), CameraError> {
        self.cs.captured.fetch_add(1, Ordering::Relaxed);
        self.last_frame = Instant::now();
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let (src_w, src_h) = (w.min(desc.Width), h.min(desc.Height));
        let tf = unpack_transform(self.cs.transform.load(Ordering::Relaxed));
        if let Some(main) = self.main.as_mut() {
            let cap = main.fps_cap().unwrap_or(self.cfg.fps).clamp(1, self.cfg.fps);
            self.pacer.set_fps(cap);
            let due = matches!(self.pacer.check(ts), Pace::Accept);
            if due && main.has_free() {
                let geo = prepare(&mut self.conv, &self.device, &desc, (src_w, src_h), &tf, tf.mirror_others, (self.cfg.width, self.cfg.height))?;
                let (ow, oh) = geo.out;
                self.out_size = (ow, oh);
                let out = self.conv.convert_slice_rect(tex, slice, geo.src).map_err(cap_err)?.clone();
                main.submit(&self.ctx, &out, ow, oh, ts).map_err(sink_err)?;
            } else {
                self.shared.drop_frames(1);
            }
        }
        if self.cs.preview_on.load(Ordering::Relaxed) {
            if let Some(p) = self.preview.as_mut() {
                if matches!(self.ppacer.check(ts), Pace::Accept) && Sink::has_free(p) {
                    let max = (self.cfg.preview_max_width, self.cfg.preview_max_height);
                    let geo = prepare(&mut self.pconv, &self.device, &desc, (src_w, src_h), &tf, tf.mirror_preview, max)?;
                    let (pw, ph) = geo.out;
                    self.preview_size = (pw, ph);
                    let out = self.pconv.convert_slice_rect(tex, slice, geo.src).map_err(cap_err)?.clone();
                    Sink::submit(p, &self.ctx, &out, pw, ph, ts).map_err(sink_err)?;
                }
            }
        }
        let ok = self.conv.transform_supported() && self.pconv.transform_supported();
        if self.cs.transform_ok.load(Ordering::Relaxed) != ok {
            self.cs.transform_ok.store(ok, Ordering::Relaxed);
        }
        self.consecutive_errors = 0;
        // Deliver whatever is already finished (never waits on the GPU).
        self.drain_main()?;
        self.drain_preview()
    }

    fn maybe_log_stats(&mut self) {
        if !self.stats_gate.ready(vox_log::STATS_INTERVAL) {
            return;
        }
        if let Some(m) = self.main.as_mut() {
            m.log_stats(&self.shared, self.out_size);
        }
        let (t0, c0, p0, d0) = self.last_stats;
        let c = self.cs.captured.load(Ordering::Relaxed);
        let p = self.cs.preview.delivered.load(Ordering::Relaxed);
        let d = self.shared.dropped.load(Ordering::Relaxed);
        let secs = t0.elapsed().as_secs_f64().max(0.001);
        tracing::info!(
            capture_fps = format!("{:.1}", (c - c0) as f64 / secs),
            preview_fps = format!("{:.1}", (p - p0) as f64 / secs),
            preview_on = self.cs.preview_on.load(Ordering::Relaxed),
            dropped = d - d0,
            gpu_frames = self.gpu_frames,
            out = format!("{}x{}", self.out_size.0, self.out_size.1),
            "camera stats"
        );
        self.last_stats = (Instant::now(), c, p, d);
    }
}

/// Crop, output size and mirror / rotation for one output, with the converter set up for them.
/// A GPU whose video processor cannot rotate keeps the picture upright (the zoom still works).
fn prepare(
    conv: &mut Converter,
    device: &ID3D11Device,
    desc: &D3D11_TEXTURE2D_DESC,
    src: (u32, u32),
    t: &CameraTransform,
    mirror: bool,
    max: (u32, u32),
) -> std::result::Result<CameraGeometry, CameraError> {
    let mut t = *t;
    let mut geo = camera_geometry(src.0, src.1, &t, max.0, max.1);
    conv.ensure(device, desc.Width, desc.Height, geo.out.0, geo.out.1).map_err(cap_err)?;
    if !conv.transform_supported() && t.rotation != CameraRotation::None {
        t.rotation = CameraRotation::None;
        geo = camera_geometry(src.0, src.1, &t, max.0, max.1);
        conv.ensure(device, desc.Width, desc.Height, geo.out.0, geo.out.1).map_err(cap_err)?;
    }
    let ok = conv.transform_supported();
    conv.set_transform(Xform { mirror: mirror && ok, rot: if ok { t.rotation.degrees() } else { 0 } });
    Ok(geo)
}

fn sink_err(e: CaptureError) -> CameraError {
    match e {
        // Encoder failures are fatal (see `Capture::handle_error`).
        CaptureError::Stopped(m) => cam_err(CameraFault::Unsupported, m),
        other => cam_err(CameraFault::Other, other.to_string()),
    }
}

/// Where the UV plane of an NV12 buffer starts: right after the Y plane, which some decoders pad
/// to a multiple of 16 rows (1080 -> 1088).
fn nv12_uv_offset(pitch: usize, h: usize, len: usize) -> usize {
    let aligned = h.div_ceil(16) * 16;
    if aligned != h && pitch > 0 && len / pitch >= aligned * 3 / 2 {
        pitch * aligned
    } else {
        pitch * h
    }
}

impl Drop for CameraLoop {
    fn drop(&mut self) {
        // The device first: the camera light goes off right away.
        let t0 = Instant::now();
        self.ctl = Controls::none();
        self.source = None;
        let released_ms = t0.elapsed().as_millis() as u64;
        self.main = None;
        self.preview = None;
        self.upload = None;
        unsafe {
            self.ctx.ClearState();
            self.ctx.Flush();
        }
        tracing::info!(released_ms, "camera released");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(fmt: Fmt, w: u32, h: u32, fps: f32) -> Mode {
        Mode { fmt, width: w, height: h, fps }
    }

    #[test]
    fn picks_720p30_mjpeg_over_slow_yuy2() {
        // A C920-like list: YUY2 720p only at 10 fps (USB 2), MJPEG 720p30.
        let modes = [
            m(Fmt::Yuy2, 640, 480, 30.0),
            m(Fmt::Yuy2, 1280, 720, 10.0),
            m(Fmt::Mjpg, 1280, 720, 30.0),
            m(Fmt::Mjpg, 1920, 1080, 30.0),
            m(Fmt::Yuy2, 1920, 1080, 5.0),
        ];
        assert_eq!(pick_mode(&modes, 1280, 720, 30), Some(2));
        assert_eq!(pick_mode(&modes, 1920, 1080, 30), Some(3));
        // 360p: a 16:9 mode beats a closer 4:3 one (tiles keep the same shape)...
        assert_eq!(pick_mode(&modes, 640, 360, 30), Some(2));
        // ... and an exact 16:9 uncompressed mode beats both.
        let mut more = modes.to_vec();
        more.push(m(Fmt::Yuy2, 640, 360, 30.0));
        assert_eq!(pick_mode(&more, 640, 360, 30), Some(5));
    }

    #[test]
    fn prefers_uncompressed_at_equal_size_and_rate() {
        let modes = [m(Fmt::Mjpg, 1280, 720, 30.0), m(Fmt::Nv12, 1280, 720, 30.0), m(Fmt::Yuy2, 1280, 720, 30.0)];
        assert_eq!(pick_mode(&modes, 1280, 720, 30), Some(1));
    }

    #[test]
    fn falls_back_to_fastest_or_biggest() {
        // Nothing reaches 30 fps: the fastest wins.
        let modes = [m(Fmt::Yuy2, 1280, 720, 10.0), m(Fmt::Yuy2, 1280, 720, 15.0)];
        assert_eq!(pick_mode(&modes, 1280, 720, 30), Some(1));
        // Only small modes: the biggest (closest) one.
        let modes = [m(Fmt::Yuy2, 320, 240, 30.0), m(Fmt::Yuy2, 640, 480, 30.0)];
        assert_eq!(pick_mode(&modes, 1280, 720, 30), Some(1));
        // 29.97 counts as 30; 60 fps only when nothing else.
        let modes = [m(Fmt::Nv12, 1280, 720, 60.0), m(Fmt::Mjpg, 1280, 720, 29.97)];
        assert_eq!(pick_mode(&modes, 1280, 720, 30), Some(1));
        assert_eq!(pick_mode(&[], 1280, 720, 30), None);
    }

    #[test]
    fn hresults_map_to_faults() {
        assert_eq!(fault_of(HRESULT(0x8007_0005_u32 as i32)), CameraFault::AccessDenied);
        assert_eq!(fault_of(HRESULT(0xC00D_3704_u32 as i32)), CameraFault::InUse);
        assert_eq!(fault_of(HRESULT(0xC00D_3EA2_u32 as i32)), CameraFault::Disconnected);
        assert_eq!(fault_of(HRESULT(0x8000_4005_u32 as i32)), CameraFault::Other);
    }

    #[test]
    fn chosen_mode_is_found_or_not() {
        let modes = [m(Fmt::Yuy2, 1280, 720, 10.0), m(Fmt::Mjpg, 1280, 720, 30.0), m(Fmt::Mjpg, 1920, 1080, 29.97)];
        let want = |w, h, fps, subtype| CameraMode { width: w, height: h, fps_milli: fps, subtype };
        assert_eq!(find_mode(&modes, &want(1280, 720, 30_000, CameraSubtype::Mjpg)), Some(1));
        assert_eq!(find_mode(&modes, &want(1280, 720, 10_000, CameraSubtype::Yuy2)), Some(0));
        // 29.97 and 30 are the same mode; another format or size is not.
        assert_eq!(find_mode(&modes, &want(1920, 1080, 30_000, CameraSubtype::Mjpg)), Some(2));
        assert_eq!(find_mode(&modes, &want(1280, 720, 30_000, CameraSubtype::Yuy2)), None);
        assert_eq!(find_mode(&modes, &want(640, 480, 30_000, CameraSubtype::Mjpg)), None);
    }

    #[test]
    fn modes_are_listed_biggest_and_fastest_first() {
        let mut v = synthetic_modes();
        v.push(v[0]);
        sort_modes(&mut v);
        assert_eq!(v.len(), synthetic_modes().len(), "duplicates dropped");
        assert_eq!((v[0].width, v[0].height), (1920, 1080));
        let pos = |w, s| v.iter().position(|m| m.width == w && m.subtype == s).unwrap();
        assert!(pos(1280, CameraSubtype::Nv12) < pos(1280, CameraSubtype::Yuy2));
        assert_eq!(v.last().map(|m| m.width), Some(640));
    }

    #[test]
    fn transform_packs_into_one_word() {
        for (mp, mo, rot, zoom) in [(true, false, 0, 100), (false, true, 90, 150), (true, true, 180, 200), (false, false, 270, 133)] {
            let t = CameraTransform { mirror_preview: mp, mirror_others: mo, rotation: CameraRotation::from_degrees(rot), zoom_pct: zoom };
            assert_eq!(unpack_transform(pack_transform(&t)), t);
        }
        assert_eq!(unpack_transform(pack_transform(&CameraTransform::default())), CameraTransform::default());
    }

    #[test]
    fn uv_plane_offset() {
        assert_eq!(nv12_uv_offset(1280, 720, 1280 * 720 * 3 / 2), 1280 * 720);
        assert_eq!(nv12_uv_offset(1920, 1080, 1920 * 1088 * 3 / 2), 1920 * 1088);
        assert_eq!(nv12_uv_offset(1920, 1080, 1920 * 1080 * 3 / 2), 1920 * 1080);
    }
}
