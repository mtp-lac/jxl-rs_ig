#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use jxl::api::{
    check_signature, Event, ExtraChannel, JxlDecoder, JxlDecoderOptions, JxlOutputBuffer,
    JxlPixelFormat, JxlSignature,
};
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};

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

/// Read-ahead granularity for streaming decodes. Metadata loads stop as soon as
/// the image headers (dimensions, extra channels, colour profile) are parsed, so
/// they only ever touch the beginning of the file. Pixel decodes stream until
/// the last frame is rendered, which keeps peak memory at the pixel buffer plus
/// one chunk and gives the cancellation poll a natural cadence.
const METADATA_CHUNK: usize = 256 * 1024;
const DECODE_CHUNK: usize = 1024 * 1024;

/// ICC profiles handed to the host through `IGImageInfo`. The ABI only
/// guarantees the host reads them synchronously inside `LoadMetadata`, so a
/// small, de-duplicated store is enough — identical profiles (the common case
/// while browsing a folder) share one allocation instead of growing a
/// process-lifetime `Vec` on every call.
const ICC_MAX_PROFILES: usize = 64;
const ICC_MAX_BYTES: usize = 4 * 1024 * 1024;

type IGStatus = i32;
const OK: IGStatus = 0;
const UNSUPPORTED: IGStatus = 1;
const CANCELED: IGStatus = 2;
const INVALID_ARG: IGStatus = 3;
const DECODE_FAILED: IGStatus = 4;
const OUT_OF_MEMORY: IGStatus = 5;
const INTERNAL: IGStatus = 6;
const IO_ERROR: IGStatus = 8;

/// Available physical memory, in bytes; `usize::MAX` if the query fails, which
/// keeps decoding rather than blocking on a platform we cannot measure.
#[cfg(target_os = "windows")]
fn available_physical_bytes() -> usize {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    extern "system" {
        fn GlobalMemoryStatusEx(lpBuffer: *mut MemoryStatusEx) -> i32;
    }
    let mut status = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        memory_load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page_file: 0,
        avail_page_file: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended_virtual: 0,
    };
    if unsafe { GlobalMemoryStatusEx(std::ptr::addr_of_mut!(status)) } != 0 {
        status.avail_phys as usize
    } else {
        usize::MAX
    }
}

/// Linux: parse `MemAvailable` from `/proc/meminfo` (reported in kB).
#[cfg(target_os = "linux")]
fn available_physical_bytes() -> usize {
    let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") else {
        return usize::MAX;
    };
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            return kb.saturating_mul(1024) as usize;
        }
    }
    usize::MAX
}

/// macOS: total physical memory via `sysctlbyname("hw.memsize")`.
#[cfg(target_os = "macos")]
fn available_physical_bytes() -> usize {
    use std::ffi::{c_char, c_int, c_void};
    extern "C" {
        fn sysctlbyname(
            name: *const c_char,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *const c_void,
            newlen: usize,
        ) -> c_int;
    }
    let mut memsize: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let ok = unsafe {
        sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut memsize as *mut u64).cast(),
            &mut len,
            std::ptr::null(),
            0,
        )
    };
    if ok == 0 && len == std::mem::size_of::<u64>() {
        memsize as usize
    } else {
        usize::MAX
    }
}

/// Other platforms: no reliable query; rely on allocation failure instead of
/// refusing up front.
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn available_physical_bytes() -> usize {
    usize::MAX
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IGStringRef {
    pub data: *const u16,
    pub length: i32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IGPixelBuffer {
    pub data: *mut u8,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub pixel_format: i32,
    pub release_context: *mut c_void,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IGImageInfo {
    pub width: i32,
    pub height: i32,
    pub pixel_format: i32,
    pub has_alpha: i32,
    pub hdr_transfer_fn: i32,
    pub color_space: i32,
    pub orientation: i32,
    pub frame_count: i32,
    pub file_size_bytes: i64,
    pub icc_profile_data: *const u8,
    pub icc_profile_size: i32,
}

#[repr(C)]
pub struct IGCodecCapability {
    pub struct_size: i32,
    pub codec_id: IGStringRef,
    pub codec_name: IGStringRef,
    pub metadata_priority: i32,
    pub decode_priority: i32,
    pub supports_metadata: i32,
    pub supports_color_profiles: i32,
    pub supports_static_raster_decoding: i32,
    pub supports_animation_decoding: i32,
    pub decode_extension_count: i32,
    pub decode_extensions: *const IGStringRef,
    pub supports_static_raster_encoding: i32,
    pub supports_multiframe_encoding: i32,
    pub encode_priority: i32,
    pub encode_extension_count: i32,
    pub encode_extensions: *const IGStringRef,
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
pub struct IGHostApi {
    pub struct_size: i32,
    pub abi_version: i32,
    pub core: *const IGHostCoreApi,
}
#[repr(C)]
pub struct IGCodecApi {
    pub struct_size: i32,
    pub get_capability: Option<unsafe extern "C" fn(*mut *mut IGCodecCapability) -> i32>,
    pub can_handle_extension: Option<unsafe extern "C" fn(IGStringRef) -> i32>,
    pub can_handle_signature: Option<unsafe extern "C" fn(*const u8, i32) -> i32>,
    pub load_metadata:
        Option<unsafe extern "C" fn(IGStringRef, *mut IGImageInfo, *mut c_void) -> i32>,
    pub decode_static_raster:
        Option<unsafe extern "C" fn(IGStringRef, i32, *mut IGPixelBuffer, *mut c_void) -> i32>,
    pub free_pixel_buffer: Option<unsafe extern "C" fn(*mut IGPixelBuffer)>,
    pub get_animation_info:
        Option<unsafe extern "C" fn(IGStringRef, *mut c_void, *mut c_void) -> i32>,
    pub free_animation_info: Option<unsafe extern "C" fn(*mut c_void)>,
    pub decode_animation_frame:
        Option<unsafe extern "C" fn(IGStringRef, i32, *mut IGPixelBuffer, *mut c_void) -> i32>,
    pub encode_static_raster: Option<
        unsafe extern "C" fn(IGStringRef, *const IGPixelBuffer, *const c_void, *mut c_void) -> i32,
    >,
    pub begin_encode_multiframe: Option<
        unsafe extern "C" fn(
            IGStringRef,
            *const c_void,
            *const c_void,
            *mut *mut c_void,
            *mut c_void,
        ) -> i32,
    >,
    pub encode_frame: Option<
        unsafe extern "C" fn(*mut c_void, *const IGPixelBuffer, *const c_void, *mut c_void) -> i32,
    >,
    pub end_encode_multiframe: Option<unsafe extern "C" fn(*mut c_void, i32, *mut c_void) -> i32>,
    pub decode_static_raster_scaled: Option<
        unsafe extern "C" fn(IGStringRef, i32, i32, i32, *mut IGPixelBuffer, *mut c_void) -> i32,
    >,
}
#[repr(C)]
pub struct IGPluginInfo {
    pub plugin_id: IGStringRef,
    pub name: IGStringRef,
    pub version: IGStringRef,
    pub abi_version: i32,
    pub codec_count: i32,
}
#[repr(C)]
pub struct IGPluginApi {
    pub struct_size: i32,
    pub abi_version: i32,
    pub info: IGPluginInfo,
    pub get_codec: unsafe extern "C" fn(i32, *mut *mut IGCodecApi) -> i32,
    pub initialize: Option<unsafe extern "C" fn() -> i32>,
    pub shutdown: Option<unsafe extern "C" fn()>,
    pub self_test: Option<unsafe extern "C" fn() -> i32>,
}

/// Incremental file reader backing the decoder. The decoder borrows a slice of
/// `buf` per `process` call; when it asks for more input we append another chunk
/// from disk. This never loads a whole file into memory just for metadata and
/// gives long decodes a natural place to poll cancellation.
struct FileFeed {
    file: File,
    length: u64,
    buf: Vec<u8>,
    consumed: usize,
    eof: bool,
}

impl FileFeed {
    fn open(path: &Path) -> std::io::Result<Self> {
        let mut file = File::open(path)?;
        let length = file.seek(std::io::SeekFrom::End(0)).unwrap_or(0);
        file.seek(std::io::SeekFrom::Start(0))?;
        Ok(Self {
            file,
            length,
            buf: Vec::new(),
            consumed: 0,
            eof: false,
        })
    }

    /// Append the next chunk (up to `chunk` bytes). Returns `Ok(false)` at EOF.
    fn read_more(&mut self, chunk: usize) -> std::io::Result<bool> {
        if self.eof || self.buf.len() as u64 >= self.length {
            self.eof = true;
            return Ok(false);
        }
        let remaining = self.length - self.buf.len() as u64;
        let take = remaining.min(chunk as u64) as usize;
        let start = self.buf.len();
        self.buf
            .try_reserve(take)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::OutOfMemory, "read buffer"))?;
        self.buf.resize(start + take, 0);
        let mut filled = 0usize;
        while filled < take {
            match self.file.read(&mut self.buf[start + filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        self.buf.truncate(start + filled);
        if filled < take {
            self.eof = true;
        }
        Ok(true)
    }
}

/// Bounded, de-duplicated ICC profile store. Boxes are heap-pinned, so their
/// pointers stay valid for as long as the entry lives; identical profiles share
/// one allocation. Entries are evicted FIFO when the store exceeds
/// `ICC_MAX_PROFILES` profiles or `ICC_MAX_BYTES` bytes.
#[derive(Default)]
struct IccStore {
    order: Vec<u64>,
    map: HashMap<u64, Box<[u8]>>,
    bytes: usize,
}

impl IccStore {
    fn retain(&mut self, profile: &[u8]) -> Option<(*const u8, usize)> {
        if profile.is_empty() {
            return None;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        profile.hash(&mut hasher);
        let key = hasher.finish();
        if let Some(existing) = self.map.get(&key) {
            return Some((existing.as_ptr(), existing.len()));
        }
        while !self.order.is_empty()
            && (self.map.len() >= ICC_MAX_PROFILES || self.bytes + profile.len() > ICC_MAX_BYTES)
        {
            let old = self.order.remove(0);
            if let Some(old_box) = self.map.remove(&old) {
                self.bytes -= old_box.len();
            }
        }
        let owned: Box<[u8]> = profile.into();
        let ptr = owned.as_ptr();
        let len = owned.len();
        self.bytes += len;
        self.order.push(key);
        self.map.insert(key, owned);
        Some((ptr, len))
    }
}

struct PluginState {
    plugin: Box<IGPluginApi>,
    codec: Box<IGCodecApi>,
    capability: Box<IGCodecCapability>,
    _strings: Vec<Vec<u16>>,
    _extensions: Box<[IGStringRef]>,
    icc: Mutex<IccStore>,
}
static STATE: OnceLock<PluginState> = OnceLock::new();
static HOST_CORE: AtomicPtr<IGHostCoreApi> = AtomicPtr::new(std::ptr::null_mut());
fn canceled(token: *mut c_void) -> bool {
    if token.is_null() {
        return false;
    }
    let core = HOST_CORE.load(Ordering::Acquire);
    if core.is_null() {
        return false;
    }
    let poll = unsafe { (*core).is_cancellation_requested };
    poll.is_some_and(|f| unsafe { f(token) != 0 })
}

impl PluginState {
    fn new() -> Self {
        let mut strings: Vec<Vec<u16>> =
            ["Plugin_JxlCodec", "JPEG XL", "1.2.0", "codec_jxl", ".jxl"]
                .iter()
                .map(|s| s.encode_utf16().collect())
                .collect();
        let refs: Vec<_> = strings
            .iter()
            .map(|s| IGStringRef {
                data: s.as_ptr(),
                length: s.len() as i32,
            })
            .collect();
        let extensions = vec![refs[4]].into_boxed_slice();
        let capability = Box::new(IGCodecCapability {
            struct_size: std::mem::size_of::<IGCodecCapability>() as i32,
            codec_id: refs[3],
            codec_name: refs[1],
            metadata_priority: 200,
            decode_priority: 200,
            supports_metadata: 1,
            supports_color_profiles: 1,
            supports_static_raster_decoding: 1,
            supports_animation_decoding: 0,
            decode_extension_count: 1,
            decode_extensions: extensions.as_ptr(),
            supports_static_raster_encoding: 0,
            supports_multiframe_encoding: 0,
            encode_priority: 0,
            encode_extension_count: 0,
            encode_extensions: std::ptr::null(),
        });
        let codec = Box::new(IGCodecApi {
            struct_size: std::mem::size_of::<IGCodecApi>() as i32,
            get_capability: Some(get_capability),
            can_handle_extension: Some(can_handle_extension),
            can_handle_signature: Some(can_handle_signature),
            load_metadata: Some(load_metadata),
            decode_static_raster: Some(decode_static_raster),
            free_pixel_buffer: Some(free_pixel_buffer),
            get_animation_info: None,
            free_animation_info: None,
            decode_animation_frame: None,
            encode_static_raster: None,
            begin_encode_multiframe: None,
            encode_frame: None,
            end_encode_multiframe: None,
            decode_static_raster_scaled: None,
        });
        let plugin = Box::new(IGPluginApi {
            struct_size: std::mem::size_of::<IGPluginApi>() as i32,
            abi_version: ABI_VERSION,
            info: IGPluginInfo {
                plugin_id: refs[0],
                name: refs[1],
                version: refs[2],
                abi_version: ABI_VERSION,
                codec_count: 1,
            },
            get_codec,
            initialize: None,
            shutdown: None,
            self_test: None,
        });
        // Keep UTF-16 buffers alive for as long as the ABI tables are reachable.
        strings.shrink_to_fit();
        Self {
            plugin,
            codec,
            capability,
            _strings: strings,
            _extensions: extensions,
            icc: Mutex::new(IccStore::default()),
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn ig_plugin_get_api(
    host_abi_version: i32,
    host_api: *const IGHostApi,
) -> *const IGPluginApi {
    if host_abi_version / 1_000_000 != 1 || host_api.is_null() {
        return std::ptr::null();
    }
    let host = unsafe { &*host_api };
    if host.struct_size < std::mem::size_of::<IGHostApi>() as i32 || host.core.is_null() {
        return std::ptr::null();
    }
    HOST_CORE.store(host.core as *mut IGHostCoreApi, Ordering::Release);
    match catch_unwind(AssertUnwindSafe(|| {
        STATE.get_or_init(PluginState::new) as *const PluginState
    })) {
        Ok(state) => unsafe { &*(*state).plugin },
        Err(_) => std::ptr::null(),
    }
}

unsafe extern "C" fn get_codec(index: i32, out: *mut *mut IGCodecApi) -> i32 {
    boundary(|| {
        if out.is_null() {
            return INVALID_ARG;
        }
        if index != 0 {
            unsafe {
                *out = std::ptr::null_mut();
            }
            return INVALID_ARG;
        }
        unsafe {
            *out = std::ptr::addr_of!(*STATE.get().expect("plugin not initialized").codec)
                as *mut IGCodecApi;
        }
        OK
    }) as i32
}
unsafe extern "C" fn get_capability(out: *mut *mut IGCodecCapability) -> i32 {
    boundary(|| {
        if out.is_null() {
            return INVALID_ARG;
        }
        unsafe {
            *out = std::ptr::addr_of!(*STATE.get().expect("plugin not initialized").capability)
                as *mut IGCodecCapability;
        }
        OK
    }) as i32
}
/// ABI contract: plain boolean int — 1 = handled, 0 = not. NOT an IGStatus.
unsafe extern "C" fn can_handle_extension(ext: IGStringRef) -> i32 {
    boundary_bool(|| {
        if ext.data.is_null() || ext.length <= 0 {
            return false;
        }
        let s = unsafe { std::slice::from_raw_parts(ext.data, ext.length as usize) };
        matches!(String::from_utf16(s), Ok(v) if v.eq_ignore_ascii_case(".jxl"))
    })
}
unsafe extern "C" fn can_handle_signature(data: *const u8, len: i32) -> i32 {
    if data.is_null() || len <= 0 {
        return 0;
    }
    match check_signature(unsafe { std::slice::from_raw_parts(data, len as usize) }) {
        JxlSignature::Codestream | JxlSignature::Container => 1,
        JxlSignature::None | JxlSignature::NeedMoreInput { .. } => 0,
    }
}

struct ParsedMeta {
    width: usize,
    height: usize,
    alpha: bool,
    animated: bool,
    icc: Option<Vec<u8>>,
}

/// Parse the JPEG XL header stream, pulling input from `feed` only as needed.
/// Stops at the first frame header, by which point dimensions, extra channels
/// and the colour profile are all known — a metadata-only load never touches the
/// pixel payload of the file. Polls cancellation at every chunk boundary.
fn decode_info(feed: &mut FileFeed, cancel: *mut c_void) -> Result<ParsedMeta, IGStatus> {
    let mut options = JxlDecoderOptions::default();
    options.sample_limit = Some(JXL_SAMPLE_LIMIT);
    let mut decoder = JxlDecoder::new(options);
    let mut saw_basic = false;
    loop {
        if canceled(cancel) {
            return Err(CANCELED);
        }
        let start = feed.consumed;
        let mut input = &feed.buf[start..];
        let event = match decoder.process(&mut input, None, None) {
            Ok(e) => e,
            Err(_) => return Err(DECODE_FAILED),
        };
        feed.consumed = (feed.buf.len() - input.len()).max(start);
        match event {
            Event::BasicInfo => saw_basic = true,
            Event::FrameHeader => {
                if !saw_basic {
                    return Err(DECODE_FAILED);
                }
                break;
            }
            Event::Complete => break,
            Event::NeedMoreInput { .. } => {
                if !feed.read_more(METADATA_CHUNK).map_err(|_| IO_ERROR)? {
                    return Err(DECODE_FAILED);
                }
            }
            Event::FrameComplete { .. } => {}
        }
    }
    let basic = decoder.basic_info().ok_or(DECODE_FAILED)?;
    let (width, height) = basic.size;
    let alpha = basic
        .extra_channels
        .iter()
        .any(|c| matches!(c.ec_type, ExtraChannel::Alpha));
    let animated = basic.animation.is_some();
    if width == 0
        || height == 0
        || width
            .checked_mul(height)
            .is_none_or(|n| n > MAX_IMAGE_PIXELS)
        || width > i32::MAX as usize
        || height > i32::MAX as usize
    {
        return Err(DECODE_FAILED);
    }
    // jxl-rs renders upright by default (`adjust_orientation = true`, and the
    // pipeline rotates the pixels), so `basic.size` is already the oriented size
    // and the host must not rotate again — always report Identity.
    // The embedded colour profile can be delivered a little after the first frame
    // header, so pull a few more chunks until we have it, hit EOF, or finish
    // rendering the first frame. Metadata loads never touch the rest of the file.
    for _ in 0..4 {
        if decoder.output_color_profile().is_some() || feed.eof {
            break;
        }
        if !feed.read_more(METADATA_CHUNK).map_err(|_| IO_ERROR)? {
            break;
        }
        let start = feed.consumed;
        let mut input = &feed.buf[start..];
        match decoder.process(&mut input, None, None) {
            Ok(Event::FrameComplete { .. }) | Ok(Event::Complete) => {
                feed.consumed = (feed.buf.len() - input.len()).max(start);
                break;
            }
            Ok(_) | Err(_) => {}
        }
        feed.consumed = (feed.buf.len() - input.len()).max(start);
    }
    let icc = decoder
        .output_color_profile()
        .and_then(|p| p.try_as_icc())
        .map(|p| p.to_vec());
    Ok(ParsedMeta {
        width,
        height,
        alpha,
        animated,
        icc,
    })
}

/// Decode the full frame into `pixels` (RGBA8), streaming the input and polling
/// cancellation at every chunk/event boundary so a long decode can be aborted.
fn decode_pixels(
    feed: &mut FileFeed,
    width: usize,
    height: usize,
    pixels: &mut [u8],
    cancel: *mut c_void,
) -> Result<(), IGStatus> {
    let mut options = JxlDecoderOptions::default();
    options.sample_limit = Some(JXL_SAMPLE_LIMIT);
    let mut decoder = JxlDecoder::new(options);
    let mut buffers: Option<[JxlOutputBuffer; 1]> = None;
    let mut frame_complete = false;
    loop {
        if canceled(cancel) {
            return Err(CANCELED);
        }
        let start = feed.consumed;
        let mut input = &feed.buf[start..];
        let event = match decoder.process(&mut input, buffers.as_mut().map(|b| &mut b[..]), None) {
            Ok(e) => e,
            Err(_) => return Err(DECODE_FAILED),
        };
        feed.consumed = (feed.buf.len() - input.len()).max(start);
        match event {
            Event::BasicInfo => {
                let channels = decoder.basic_info().map_or(0, |b| b.extra_channels.len());
                decoder
                    .set_pixel_format(JxlPixelFormat::rgba8(channels))
                    .map_err(|_| DECODE_FAILED)?;
                buffers = Some([JxlOutputBuffer::new(pixels, height, width * 4)]);
            }
            Event::FrameComplete { .. } => frame_complete = true,
            Event::Complete => break,
            Event::FrameHeader => {}
            Event::NeedMoreInput { .. } => {
                if !feed.read_more(DECODE_CHUNK).map_err(|_| IO_ERROR)? {
                    return Err(DECODE_FAILED);
                }
            }
        }
    }
    if frame_complete {
        Ok(())
    } else {
        Err(DECODE_FAILED)
    }
}

unsafe extern "C" fn load_metadata(
    path: IGStringRef,
    out: *mut IGImageInfo,
    cancel: *mut c_void,
) -> i32 {
    boundary(|| {
        if path.data.is_null() || path.length < 0 || out.is_null() {
            return INVALID_ARG;
        }
        if canceled(cancel) {
            return CANCELED;
        }
        let path = match string_path(path) {
            Ok(p) => p,
            Err(s) => return s,
        };
        let mut feed = match FileFeed::open(&path) {
            Ok(f) => f,
            Err(_) => return IO_ERROR,
        };
        let meta = match decode_info(&mut feed, cancel) {
            Ok(m) => m,
            Err(s) => return s,
        };
        if meta.animated {
            return UNSUPPORTED;
        }
        let (icc_ptr, icc_len) = match &meta.icc {
            Some(profile) => {
                let Ok(mut store) = STATE.get().expect("plugin initialized").icc.lock() else {
                    return INTERNAL;
                };
                match store.retain(profile) {
                    Some(p) => p,
                    None => return INTERNAL,
                }
            }
            None => (std::ptr::null(), 0),
        };
        let file_size = std::fs::metadata(&path)
            .map(|m| m.len() as i64)
            .unwrap_or(-1);
        unsafe {
            *out = IGImageInfo {
                width: meta.width as i32,
                height: meta.height as i32,
                pixel_format: PIXEL_BGRA8,
                has_alpha: meta.alpha as i32,
                hdr_transfer_fn: 0,
                color_space: 0,
                orientation: 1,
                frame_count: 1,
                file_size_bytes: file_size,
                icc_profile_data: icc_ptr,
                icc_profile_size: icc_len.min(i32::MAX as usize) as i32,
            };
        }
        OK
    }) as i32
}
unsafe extern "C" fn decode_static_raster(
    path: IGStringRef,
    frame: i32,
    out: *mut IGPixelBuffer,
    cancel: *mut c_void,
) -> i32 {
    boundary(|| {
        if path.data.is_null() || path.length < 0 || out.is_null() || frame != 0 {
            return INVALID_ARG;
        }
        unsafe {
            *out = IGPixelBuffer::default();
        }
        if canceled(cancel) {
            return CANCELED;
        }
        let path = match string_path(path) {
            Ok(p) => p,
            Err(s) => return s,
        };
        let mut feed = match FileFeed::open(&path) {
            Ok(f) => f,
            Err(_) => return IO_ERROR,
        };
        let meta = match decode_info(&mut feed, cancel) {
            Ok(m) => m,
            Err(s) => return s,
        };
        if meta.animated {
            return UNSUPPORTED;
        }
        let bytes = match meta
            .width
            .checked_mul(meta.height)
            .and_then(|n| n.checked_mul(4))
        {
            Some(n) if n <= MAX_OUTPUT_BYTES => n,
            _ => return DECODE_FAILED,
        };
        // A 32000x32000 frame alone is ~4 GB, and jxl-rs plus the host's own copy
        // need more on top. Refuse into OutOfMemory rather than paging the host.
        let required = bytes
            .saturating_add(bytes / 2)
            .saturating_add(64 * 1024 * 1024);
        if required > available_physical_bytes() {
            return OUT_OF_MEMORY;
        }
        let mut pixels = Vec::new();
        if pixels.try_reserve_exact(bytes).is_err() {
            return OUT_OF_MEMORY;
        }
        pixels.resize(bytes, 0);
        let mut feed = match FileFeed::open(&path) {
            Ok(f) => f,
            Err(_) => return IO_ERROR,
        };
        if decode_pixels(&mut feed, meta.width, meta.height, &mut pixels, cancel).is_err() {
            return DECODE_FAILED;
        }
        let (rgba_chunks, remainder) = pixels.as_chunks_mut::<4>();
        debug_assert!(remainder.is_empty());
        for px in rgba_chunks {
            px.swap(0, 2);
        }
        let allocation = Box::new(pixels);
        let raw = allocation.as_ptr() as *mut u8;
        let release_context = Box::into_raw(allocation) as *mut c_void;
        unsafe {
            *out = IGPixelBuffer {
                data: raw,
                width: meta.width as i32,
                height: meta.height as i32,
                stride: (meta.width * 4) as i32,
                pixel_format: PIXEL_BGRA8,
                release_context,
            };
        }
        OK
    }) as i32
}
unsafe extern "C" fn free_pixel_buffer(buf: *mut IGPixelBuffer) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if buf.is_null() {
            return;
        }
        let b = unsafe { &mut *buf };
        if !b.release_context.is_null() {
            unsafe {
                drop(Box::from_raw(b.release_context as *mut Vec<u8>));
            }
        }
        *b = IGPixelBuffer::default();
    }));
}

fn string_path(s: IGStringRef) -> Result<std::path::PathBuf, IGStatus> {
    let units = unsafe { std::slice::from_raw_parts(s.data, s.length as usize) };
    String::from_utf16(units)
        .map(std::path::PathBuf::from)
        .map_err(|_| INVALID_ARG)
}
fn boundary(f: impl FnOnce() -> IGStatus) -> IGStatus {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(status) => status,
        Err(_) => INTERNAL,
    }
}
/// Panic boundary for the two boolean probes (CanHandleExtension/Signature),
/// which return 1/0 rather than an IGStatus.
fn boundary_bool(f: impl FnOnce() -> bool) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(true) => 1,
        Ok(false) | Err(_) => 0,
    }
}
