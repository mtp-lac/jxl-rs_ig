#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use jxl::api::{JxlDecoder, JxlDecoderOptions, JxlOutputBuffer, JxlPixelFormat, ProcessingResult, check_signature};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;

unsafe impl Sync for PluginState {}
unsafe impl Send for PluginState {}

const ABI_VERSION: i32 = 1_002_000;
const PIXEL_BGRA8: i32 = 1;
/// Largest decoded frame the plugin will accept, in pixels. Covers 32000x32000
/// as required; BGRA8 output for a frame at this cap is ~4.3 GB, so allocation
/// goes through `try_reserve` and failure is reported as OutOfMemory, never abort.
const MAX_IMAGE_PIXELS: usize = 1_024_000_000; // 32_000 * 32_000
const MAX_OUTPUT_BYTES: usize = MAX_IMAGE_PIXELS * 4;

/// jxl-rs `sample_limit` counts `pixels * (3 + extra_channels)` and rejects when
/// `total >= limit` (see `check_size_limit` in the crate). For a 4-sample image
/// (RGB + alpha) this limit enforces at most `MAX_IMAGE_PIXELS` pixels; images
/// with extra channels are bounded proportionally tighter. The +1 keeps the
/// exact cap admissible given the decoder's `>=` comparison.
const JXL_SAMPLE_LIMIT: usize = MAX_OUTPUT_BYTES + 1;

type IGStatus = i32;
const OK: IGStatus = 0;
const UNSUPPORTED: IGStatus = 1;
const CANCELED: IGStatus = 2;
const INVALID_ARG: IGStatus = 3;
const DECODE_FAILED: IGStatus = 4;
const OUT_OF_MEMORY: IGStatus = 5;
const INTERNAL: IGStatus = 6;
const IO_ERROR: IGStatus = 8;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IGStringRef { pub data: *const u16, pub length: i32 }
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IGPixelBuffer { pub data: *mut u8, pub width: i32, pub height: i32, pub stride: i32, pub pixel_format: i32, pub release_context: *mut c_void }
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IGImageInfo { pub width: i32, pub height: i32, pub pixel_format: i32, pub has_alpha: i32, pub hdr_transfer_fn: i32, pub color_space: i32, pub orientation: i32, pub frame_count: i32, pub file_size_bytes: i64, pub icc_profile_data: *const u8, pub icc_profile_size: i32 }

#[repr(C)]
pub struct IGCodecCapability {
    pub struct_size: i32, pub codec_id: IGStringRef, pub codec_name: IGStringRef,
    pub metadata_priority: i32, pub decode_priority: i32,
    pub supports_metadata: i32, pub supports_color_profiles: i32, pub supports_static_raster_decoding: i32,
    pub supports_animation_decoding: i32, pub decode_extension_count: i32,
    pub decode_extensions: *const IGStringRef, pub supports_static_raster_encoding: i32,
    pub supports_multiframe_encoding: i32, pub encode_priority: i32,
    pub encode_extension_count: i32, pub encode_extensions: *const IGStringRef,
}
#[repr(C)]
pub struct IGHostCoreApi {
    pub log: Option<unsafe extern "C" fn(i32, IGStringRef)>,
    pub alloc: Option<unsafe extern "C" fn(usize) -> *mut c_void>,
    pub free: Option<unsafe extern "C" fn(*mut c_void)>,
    pub is_cancellation_requested: Option<unsafe extern "C" fn(*mut c_void) -> i32>,
    pub get_config_directory: Option<unsafe extern "C" fn(*mut u16, i32) -> i32>,
}
#[repr(C)]
pub struct IGHostApi { pub struct_size: i32, pub abi_version: i32, pub core: *const IGHostCoreApi }
#[repr(C)]
pub struct IGCodecApi {
    pub struct_size: i32,
    pub get_capability: Option<unsafe extern "C" fn(*mut *mut IGCodecCapability) -> i32>,
    pub can_handle_extension: Option<unsafe extern "C" fn(IGStringRef) -> i32>,
    pub can_handle_signature: Option<unsafe extern "C" fn(*const u8, i32) -> i32>,
    pub load_metadata: Option<unsafe extern "C" fn(IGStringRef, *mut IGImageInfo, *mut c_void) -> i32>,
    pub decode_static_raster: Option<unsafe extern "C" fn(IGStringRef, i32, *mut IGPixelBuffer, *mut c_void) -> i32>,
    pub free_pixel_buffer: Option<unsafe extern "C" fn(*mut IGPixelBuffer)>,
    pub get_animation_info: Option<unsafe extern "C" fn(IGStringRef, *mut c_void, *mut c_void) -> i32>,
    pub free_animation_info: Option<unsafe extern "C" fn(*mut c_void)>,
    pub decode_animation_frame: Option<unsafe extern "C" fn(IGStringRef, i32, *mut IGPixelBuffer, *mut c_void) -> i32>,
    pub encode_static_raster: Option<unsafe extern "C" fn(IGStringRef, *const IGPixelBuffer, *const c_void, *mut c_void) -> i32>,
    pub begin_encode_multiframe: Option<unsafe extern "C" fn(IGStringRef, *const c_void, *const c_void, *mut *mut c_void, *mut c_void) -> i32>,
    pub encode_frame: Option<unsafe extern "C" fn(*mut c_void, *const IGPixelBuffer, *const c_void, *mut c_void) -> i32>,
    pub end_encode_multiframe: Option<unsafe extern "C" fn(*mut c_void, i32, *mut c_void) -> i32>,
    pub decode_static_raster_scaled: Option<unsafe extern "C" fn(IGStringRef, i32, i32, i32, *mut IGPixelBuffer, *mut c_void) -> i32>,
}
#[repr(C)]
pub struct IGPluginInfo { pub plugin_id: IGStringRef, pub name: IGStringRef, pub version: IGStringRef, pub abi_version: i32, pub codec_count: i32 }
#[repr(C)]
pub struct IGPluginApi { pub struct_size: i32, pub abi_version: i32, pub info: IGPluginInfo, pub get_codec: unsafe extern "C" fn(i32, *mut *mut IGCodecApi) -> i32, pub initialize: Option<unsafe extern "C" fn() -> i32>, pub shutdown: Option<unsafe extern "C" fn()>, pub self_test: Option<unsafe extern "C" fn() -> i32> }

struct PluginState { plugin: Box<IGPluginApi>, codec: Box<IGCodecApi>, capability: Box<IGCodecCapability>, _strings: Vec<Vec<u16>>, _extensions: Box<[IGStringRef]>, _icc_profiles: std::sync::Mutex<Vec<Box<[u8]>>> }
static STATE: OnceLock<PluginState> = OnceLock::new();
static HOST_CORE: std::sync::atomic::AtomicPtr<IGHostCoreApi> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
fn canceled(token: *mut c_void) -> bool {
    if token.is_null() { return false; }
    let core = HOST_CORE.load(std::sync::atomic::Ordering::Acquire);
    if core.is_null() { return false; }
    let poll = unsafe { (*core).is_cancellation_requested };
    poll.is_some_and(|f| unsafe { f(token) != 0 })
}

impl PluginState {
    fn new() -> Self {
        let mut strings: Vec<Vec<u16>> = ["Plugin_JxlCodec", "JPEG XL", "1.0.0", "codec_jxl", ".jxl"].iter().map(|s| s.encode_utf16().collect()).collect();
        let refs: Vec<_> = strings.iter().map(|s| IGStringRef { data: s.as_ptr(), length: s.len() as i32 }).collect();
        let extensions = vec![refs[4]].into_boxed_slice();
        let capability = Box::new(IGCodecCapability {
            struct_size: std::mem::size_of::<IGCodecCapability>() as i32, codec_id: refs[3], codec_name: refs[1],
            metadata_priority: 200, decode_priority: 200,
            supports_metadata: 1, supports_color_profiles: 1, supports_static_raster_decoding: 1,
            supports_animation_decoding: 0, decode_extension_count: 1, decode_extensions: extensions.as_ptr(),
            supports_static_raster_encoding: 0, supports_multiframe_encoding: 0, encode_priority: 0, encode_extension_count: 0, encode_extensions: std::ptr::null(),
        });
        let codec = Box::new(IGCodecApi {
            struct_size: std::mem::size_of::<IGCodecApi>() as i32, get_capability: Some(get_capability),
            can_handle_extension: Some(can_handle_extension), can_handle_signature: Some(can_handle_signature),
            load_metadata: Some(load_metadata), decode_static_raster: Some(decode_static_raster), free_pixel_buffer: Some(free_pixel_buffer),
            get_animation_info: None, free_animation_info: None, decode_animation_frame: None, encode_static_raster: None,
            begin_encode_multiframe: None, encode_frame: None, end_encode_multiframe: None, decode_static_raster_scaled: None,
        });
        let plugin = Box::new(IGPluginApi {
            struct_size: std::mem::size_of::<IGPluginApi>() as i32, abi_version: ABI_VERSION,
            info: IGPluginInfo { plugin_id: refs[0], name: refs[1], version: refs[2], abi_version: ABI_VERSION, codec_count: 1 },
            get_codec, initialize: None, shutdown: None, self_test: None,
        });
        // Keep UTF-16 buffers alive for as long as the ABI tables are reachable.
        strings.shrink_to_fit();
        Self { plugin, codec, capability, _strings: strings, _extensions: extensions, _icc_profiles: std::sync::Mutex::new(Vec::new()) }
    }
}

#[no_mangle]
pub unsafe extern "C" fn ig_plugin_get_api(host_abi_version: i32, host_api: *const IGHostApi) -> *const IGPluginApi {
    if host_abi_version / 1_000_000 != 1 || host_api.is_null() { return std::ptr::null(); }
    let host = unsafe { &*host_api };
    if host.struct_size < std::mem::size_of::<IGHostApi>() as i32 || host.core.is_null() { return std::ptr::null(); }
    HOST_CORE.store(host.core as *mut IGHostCoreApi, std::sync::atomic::Ordering::Release);
    match catch_unwind(AssertUnwindSafe(|| STATE.get_or_init(PluginState::new) as *const PluginState)) {
        Ok(state) => unsafe { &*(*state).plugin },
        Err(_) => std::ptr::null(),
    }
}

unsafe extern "C" fn get_codec(index: i32, out: *mut *mut IGCodecApi) -> i32 {
    boundary(|| {
        if out.is_null() { return INVALID_ARG; }
        if index != 0 { unsafe { *out = std::ptr::null_mut(); } return INVALID_ARG; }
        unsafe { *out = std::ptr::addr_of!(*STATE.get().expect("plugin not initialized").codec) as *mut IGCodecApi; }
        OK
    }) as i32
}
unsafe extern "C" fn get_capability(out: *mut *mut IGCodecCapability) -> i32 {
    boundary(|| { if out.is_null() { return INVALID_ARG; } unsafe { *out = std::ptr::addr_of!(*STATE.get().expect("plugin not initialized").capability) as *mut IGCodecCapability; } OK }) as i32
}
unsafe extern "C" fn can_handle_extension(ext: IGStringRef) -> i32 {
    boundary(|| {
        if ext.data.is_null() || ext.length <= 0 { return UNSUPPORTED; }
        let s = unsafe { std::slice::from_raw_parts(ext.data, ext.length as usize) };
        match String::from_utf16(s) { Ok(v) if v.eq_ignore_ascii_case(".jxl") => OK, _ => UNSUPPORTED }
    }) as i32
}
unsafe extern "C" fn can_handle_signature(data: *const u8, len: i32) -> i32 {
    if data.is_null() || len <= 0 { return 0; }
    match check_signature(unsafe { std::slice::from_raw_parts(data, len as usize) }) {
        ProcessingResult::Complete { result: Some(_) } => 1,
        ProcessingResult::Complete { result: None } => 0,
        ProcessingResult::NeedsMoreInput { .. } => 0,
    }
}
unsafe extern "C" fn load_metadata(path: IGStringRef, out: *mut IGImageInfo, cancel: *mut c_void) -> i32 {
    boundary(|| {
        if path.data.is_null() || path.length < 0 || out.is_null() { return INVALID_ARG; }
        if canceled(cancel) { return CANCELED; }
        let path = match string_path(path) { Ok(p) => p, Err(s) => return s };
        let data = match std::fs::read(path) { Ok(v) => v, Err(_) => return IO_ERROR };
        let info = match decode_info(&data) { Ok(v) => v, Err(s) => return s };
        if info.4 { return UNSUPPORTED; }
        let (icc_ptr, icc_len) = if let Some(profile) = info.3 {
            let profile = profile.into_boxed_slice();
            let ptr = profile.as_ptr();
            let len = profile.len();
            if let Ok(mut profiles) = STATE.get().expect("plugin initialized")._icc_profiles.lock() { profiles.push(profile); } else { return INTERNAL; }
            (ptr, len)
        } else { (std::ptr::null(), 0) };
        unsafe { *out = IGImageInfo { width: info.0 as i32, height: info.1 as i32, pixel_format: PIXEL_BGRA8, has_alpha: info.2 as i32, hdr_transfer_fn: 0, color_space: 0, orientation: info.5, frame_count: 1, file_size_bytes: data.len() as i64, icc_profile_data: icc_ptr, icc_profile_size: icc_len.min(i32::MAX as usize) as i32 }; }
        OK
    }) as i32
}
unsafe extern "C" fn decode_static_raster(path: IGStringRef, frame: i32, out: *mut IGPixelBuffer, cancel: *mut c_void) -> i32 {
    boundary(|| {
        if path.data.is_null() || path.length < 0 || out.is_null() || frame != 0 { return INVALID_ARG; }
        unsafe { *out = IGPixelBuffer::default(); }
        if canceled(cancel) { return CANCELED; }
        let path = match string_path(path) { Ok(p) => p, Err(s) => return s };
        let data = match std::fs::read(path) { Ok(v) => v, Err(_) => return IO_ERROR };
        let (width, height, _, _, animated, _) = match decode_info(&data) { Ok(v) => v, Err(s) => return s };
        if animated { return UNSUPPORTED; }
        let bytes = match width.checked_mul(height).and_then(|n| n.checked_mul(4)) { Some(n) if n <= MAX_OUTPUT_BYTES => n, _ => return DECODE_FAILED };
        let mut pixels = Vec::new();
        if pixels.try_reserve_exact(bytes).is_err() { return OUT_OF_MEMORY; }
        pixels.resize(bytes, 0);
        if decode_pixels(&data, width, height, &mut pixels).is_err() { return DECODE_FAILED; }
        for px in pixels.chunks_exact_mut(4) { px.swap(0, 2); }
        let allocation = Box::new(pixels);
        let raw = allocation.as_ptr() as *mut u8;
        let release_context = Box::into_raw(allocation) as *mut c_void;
        unsafe { *out = IGPixelBuffer { data: raw, width: width as i32, height: height as i32, stride: (width * 4) as i32, pixel_format: PIXEL_BGRA8, release_context }; }
        OK
    }) as i32
}
unsafe extern "C" fn free_pixel_buffer(buf: *mut IGPixelBuffer) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if buf.is_null() { return; }
        let b = unsafe { &mut *buf };
        if !b.release_context.is_null() {
            unsafe { drop(Box::from_raw(b.release_context as *mut Vec<u8>)); }
        }
        *b = IGPixelBuffer::default();
    }));
}

fn string_path(s: IGStringRef) -> Result<std::path::PathBuf, IGStatus> {
    let units = unsafe { std::slice::from_raw_parts(s.data, s.length as usize) };
    String::from_utf16(units).map(std::path::PathBuf::from).map_err(|_| INVALID_ARG)
}
fn decode_info(data: &[u8]) -> Result<(usize, usize, bool, Option<Vec<u8>>, bool, i32), IGStatus> {
    let mut options = JxlDecoderOptions::default();
    options.sample_limit = Some(JXL_SAMPLE_LIMIT);
    let decoder = JxlDecoder::new(options);
    let mut input = data;
    let decoder = match decoder.process(&mut input, None).map_err(|_| DECODE_FAILED)? {
        ProcessingResult::Complete { result } => result,
        ProcessingResult::NeedsMoreInput { .. } => return Err(DECODE_FAILED),
    };
    let basic = decoder.basic_info();
    let (w, h) = basic.size;
    let has_alpha = basic.extra_channels.iter().any(|c| matches!(c.ec_type, jxl::headers::extra_channels::ExtraChannel::Alpha));
    let animated = basic.animation.is_some();
    let orientation = basic.orientation as i32;
    let icc = decoder.output_color_profile().try_as_icc().map(|p| p.to_vec());
    if w == 0 || h == 0 || w.checked_mul(h).is_none_or(|n| n > MAX_IMAGE_PIXELS) || w > i32::MAX as usize || h > i32::MAX as usize { return Err(DECODE_FAILED); }
    Ok((w, h, has_alpha, icc, animated, orientation))
}
fn decode_pixels(data: &[u8], width: usize, height: usize, pixels: &mut [u8]) -> Result<(), ()> {
    let mut options = JxlDecoderOptions::default();
    options.sample_limit = Some(JXL_SAMPLE_LIMIT);
    let decoder = JxlDecoder::new(options);
    let mut input = data;
    let mut decoder = match decoder.process(&mut input, None).map_err(|_| ())? {
        ProcessingResult::Complete { result } => result,
        ProcessingResult::NeedsMoreInput { .. } => return Err(()),
    };
    let channels = decoder.basic_info().extra_channels.len();
    decoder.set_pixel_format(JxlPixelFormat::rgba8(channels)).map_err(|_| ())?;
    let frame_decoder = match decoder.process(&mut input, None).map_err(|_| ())? {
        ProcessingResult::Complete { result } => result,
        ProcessingResult::NeedsMoreInput { .. } => return Err(()),
    };
    let mut buffers = [JxlOutputBuffer::new(pixels, height, width * 4)];
    match frame_decoder.process(&mut input, &mut buffers, None).map_err(|_| ())? {
        ProcessingResult::Complete { .. } => Ok(()),
        ProcessingResult::NeedsMoreInput { .. } => Err(()),
    }
}
fn boundary(f: impl FnOnce() -> IGStatus) -> IGStatus {
    match catch_unwind(AssertUnwindSafe(f)) { Ok(status) => status, Err(_) => INTERNAL }
}
