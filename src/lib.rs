#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use jxl::api::{
    check_signature, AlphaMode, Event, ExtraChannel, JxlBasicInfo, JxlBitstreamInput,
    JxlColorEncoding, JxlColorProfile, JxlColorType, JxlDataFormat, JxlDecoder, JxlDecoderOptions,
    JxlOutputBuffer, JxlParallelRunner, JxlParallelRunnerFun, JxlPixelFormat, JxlSignature,
};
use jxl::error::Error as JxlError;
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{IoSliceMut, Read, Seek, SeekFrom};
use std::ops::{Deref, DerefMut};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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

/// Read-ahead buffer capacity and cancellation polling cadence for streaming
/// `JxlBitstreamInput` I/O. Metadata loads only read a small prefix before
/// stopping at `Event::BasicInfo`; pixel decodes expand to 1 MiB buffering (or
/// direct reads into large section buffers) and poll cancellation every 1 MiB.
const METADATA_READ_BUF: usize = 16 * 1024;
const DECODE_READ_BUF: usize = 1024 * 1024;
const CANCEL_POLL_BYTES: usize = 1024 * 1024;

/// 64-byte alignment for output pixel buffers (`AlignedBuffer` + `alloc_zeroed`).
const PIXEL_BUFFER_ALIGN: usize = 64;

/// Bounded `JxlDecoder` reuse pool. Retains `BufferRecycler` buckets and thread
/// scratch buffers across consecutive views without holding huge 16k/32k buffers.
const MAX_POOLED_DECODERS: usize = 4;
const DECODER_POOL_MAX_PIXELS: usize = 64 * 1024 * 1024;

/// ICC profiles handed to the host through `IGImageInfo`. ImageGlass copies the
/// profile bytes after `LoadMetadata` returns and may issue concurrent metadata
/// loads, so evicted buffers are held in a bounded retirement queue for
/// `ICC_RETIRE_GRACE` before being freed. Individual profiles above
/// `ICC_MAX_BYTES` are not retained.
const ICC_MAX_PROFILES: usize = 64;
const ICC_MAX_BYTES: usize = 4 * 1024 * 1024;
const ICC_RETIRE_GRACE: Duration = Duration::from_secs(10);
const ICC_RETIRED_MAX_BYTES: usize = 16 * 1024 * 1024;

type IGStatus = i32;
const OK: IGStatus = 0;
const UNSUPPORTED: IGStatus = 1;
const CANCELED: IGStatus = 2;
const INVALID_ARG: IGStatus = 3;
const DECODE_FAILED: IGStatus = 4;
const OUT_OF_MEMORY: IGStatus = 5;
const INTERNAL: IGStatus = 6;
const IO_ERROR: IGStatus = 8;

const LOG_WARN: i32 = 1;
const LOG_ERROR: i32 = 2;

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

/// macOS: available physical memory via Mach `host_statistics64(HOST_VM_INFO64)`.
#[cfg(target_os = "macos")]
fn available_physical_bytes() -> usize {
    use std::ffi::c_int;
    const HOST_VM_INFO64: c_int = 4;
    const KERN_SUCCESS: c_int = 0;

    #[repr(C)]
    #[derive(Default)]
    #[allow(dead_code)]
    struct VmStatistics64 {
        free_count: u32,
        active_count: u32,
        inactive_count: u32,
        wire_count: u32,
        zero_fill_count: u64,
        reactivations: u64,
        pageins: u64,
        pageouts: u64,
        faults: u64,
        cow_faults: u64,
        lookups: u64,
        hits: u64,
        purges: u64,
        purgeable_count: u32,
        speculative_count: u32,
        decompressions: u64,
        compressions: u64,
        swapins: u64,
        swapouts: u64,
        compressor_page_count: u32,
        throttled_count: u32,
        external_page_count: u32,
        internal_page_count: u32,
        total_uncompressed_pages_in_compressor: u64,
    }

    extern "C" {
        fn mach_host_self() -> u32;
        fn host_page_size(host: u32, out_page_size: *mut usize) -> c_int;
        fn host_statistics64(
            host_priv: u32,
            flavor: c_int,
            host_info_out: *mut i32,
            host_info_out_cnt: *mut u32,
        ) -> c_int;
    }

    let host = unsafe { mach_host_self() };
    let mut page_size: usize = 0;
    if unsafe { host_page_size(host, &mut page_size) } != KERN_SUCCESS || page_size == 0 {
        return usize::MAX;
    }
    let mut stats = VmStatistics64::default();
    let mut count = (std::mem::size_of::<VmStatistics64>() / std::mem::size_of::<u32>()) as u32;
    let ok = unsafe {
        host_statistics64(
            host,
            HOST_VM_INFO64,
            std::ptr::addr_of_mut!(stats).cast(),
            &mut count,
        )
    };
    if ok == KERN_SUCCESS {
        let avail_pages = (stats.free_count as u64)
            .saturating_add((stats.inactive_count as u64).max(stats.purgeable_count as u64));
        avail_pages
            .saturating_mul(page_size as u64)
            .min(usize::MAX as u64) as usize
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

struct FileReader {
    file: File,
    eof: bool,
    cancel: *mut c_void,
    bytes_since_poll: usize,
    was_canceled: bool,
    io_err: bool,
}

impl FileReader {
    fn read_raw(&mut self, dst: &mut [u8]) -> std::io::Result<usize> {
        if self.bytes_since_poll >= CANCEL_POLL_BYTES {
            self.bytes_since_poll = 0;
            if canceled(self.cancel) {
                self.was_canceled = true;
                return Err(std::io::Error::other("canceled"));
            }
        }
        loop {
            match self.file.read(dst) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(0);
                }
                Ok(n) => {
                    self.bytes_since_poll += n;
                    return Ok(n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                    if canceled(self.cancel) {
                        self.was_canceled = true;
                        return Err(std::io::Error::other("canceled"));
                    }
                }
                Err(e) => {
                    self.io_err = true;
                    return Err(e);
                }
            }
        }
    }
}

/// Direct streaming `JxlBitstreamInput` backed by `File`.
///
/// Reports the full remaining file length via `available_bytes()` so `jxl-rs`
/// can batch all sections across worker threads, implements `skip()` with
/// `O(1)` file seeks, and fills all `IoSliceMut` slices per `read()` call
/// without keeping the entire file resident in a growing `Vec<u8>`.
struct FileInput {
    reader: FileReader,
    length: u64,
    pos: u64,
    buf: Vec<u8>,
    buf_cap: usize,
    cursor: usize,
    filled: usize,
}

impl FileInput {
    fn open(path: &Path, buf_cap: usize, cancel: *mut c_void) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let length = file.metadata()?.len();
        Ok(Self {
            reader: FileReader {
                file,
                eof: length == 0,
                cancel,
                bytes_since_poll: 0,
                was_canceled: false,
                io_err: false,
            },
            length,
            pos: 0,
            buf: Vec::new(),
            buf_cap,
            cursor: 0,
            filled: 0,
        })
    }

    fn set_decode_mode(&mut self) {
        self.buf_cap = DECODE_READ_BUF;
    }

    fn compressed_margin(&self) -> usize {
        self.length.min(usize::MAX as u64) as usize
    }

    fn is_exhausted(&self) -> bool {
        self.cursor >= self.filled && (self.reader.eof || self.pos >= self.length)
    }

    fn rewind_to_start(&mut self) -> std::io::Result<()> {
        if self.pos <= self.cursor as u64 {
            self.cursor -= self.pos as usize;
            self.pos = 0;
            return Ok(());
        }
        self.reader.file.seek(SeekFrom::Start(0))?;
        self.reader.eof = self.length == 0;
        self.pos = 0;
        self.cursor = 0;
        self.filled = 0;
        Ok(())
    }
}

impl JxlBitstreamInput for FileInput {
    fn available_bytes(&mut self) -> std::io::Result<usize> {
        if self.cursor >= self.filled && (self.reader.eof || self.pos >= self.length) {
            Ok(0)
        } else {
            Ok(self.length.saturating_sub(self.pos).min(usize::MAX as u64) as usize)
        }
    }

    fn read(&mut self, bufs: &mut [IoSliceMut]) -> std::io::Result<usize> {
        let mut total = 0usize;
        for slice in bufs {
            let mut offset = 0usize;
            while offset < slice.len() {
                let avail = self.filled - self.cursor;
                if avail > 0 {
                    let take = avail.min(slice.len() - offset);
                    slice[offset..offset + take]
                        .copy_from_slice(&self.buf[self.cursor..self.cursor + take]);
                    self.cursor += take;
                    self.pos += take as u64;
                    offset += take;
                    total += take;
                    if self.cursor >= self.filled {
                        self.cursor = 0;
                        self.filled = 0;
                    }
                    continue;
                }
                if self.reader.eof || self.pos >= self.length {
                    self.reader.eof = true;
                    return Ok(total);
                }
                let rem_limit = self.length.saturating_sub(self.pos);
                let rem_slice = slice.len() - offset;
                if rem_slice >= self.buf_cap {
                    let chunk_len = (rem_slice as u64)
                        .min(rem_limit)
                        .min(CANCEL_POLL_BYTES as u64) as usize;
                    let n = self
                        .reader
                        .read_raw(&mut slice[offset..offset + chunk_len])?;
                    if n == 0 {
                        return Ok(total);
                    }
                    self.pos += n as u64;
                    offset += n;
                    total += n;
                    continue;
                }
                let disk_rem = rem_limit.min(usize::MAX as u64) as usize;
                let want = self.buf_cap.min(disk_rem.max(4096));
                if self.buf.len() < want {
                    self.buf
                        .try_reserve_exact(want - self.buf.len())
                        .map_err(|_| {
                            std::io::Error::new(std::io::ErrorKind::OutOfMemory, "read buffer")
                        })?;
                    self.buf.resize(want, 0);
                }
                let n = self.reader.read_raw(&mut self.buf[..want])?;
                if n == 0 {
                    return Ok(total);
                }
                self.cursor = 0;
                self.filled = n;
            }
        }
        Ok(total)
    }

    fn skip(&mut self, bytes: usize) -> std::io::Result<usize> {
        let avail = self.filled - self.cursor;
        let from_buf = bytes.min(avail);
        if from_buf > 0 {
            self.cursor += from_buf;
            self.pos += from_buf as u64;
            if self.cursor >= self.filled {
                self.cursor = 0;
                self.filled = 0;
            }
        }
        let rem = bytes - from_buf;
        if rem == 0 {
            return Ok(from_buf);
        }
        let to_seek = (rem as u64).min(self.length.saturating_sub(self.pos));
        if to_seek > 0 {
            if let Err(e) = self.reader.file.seek(SeekFrom::Start(self.pos + to_seek)) {
                self.reader.io_err = true;
                return Err(e);
            }
            self.pos += to_seek;
        }
        if self.pos >= self.length {
            self.reader.eof = true;
        }
        Ok(from_buf + to_seek as usize)
    }
}

/// Bounded, de-duplicated ICC profile store. Boxes are heap-pinned, so their
/// pointers stay valid for as long as the entry lives; identical profiles share
/// one allocation. Entries are evicted LRU when the store exceeds
/// `ICC_MAX_PROFILES` profiles or `ICC_MAX_BYTES` bytes, and evicted buffers are
/// held in `retired` for `ICC_RETIRE_GRACE` so concurrent host copies cannot
/// read freed memory. Also caches simple `JxlColorEncoding` descriptors to avoid
/// regenerating ICC bytes and MD5 sums.
#[derive(Default)]
struct IccStore {
    order: VecDeque<u64>,
    map: HashMap<u64, Box<[u8]>>,
    simple: Vec<(JxlColorEncoding, u32, u64)>,
    bytes: usize,
    retired: VecDeque<(Instant, Box<[u8]>)>,
    retired_bytes: usize,
}

impl IccStore {
    fn clear(&mut self) {
        self.order.clear();
        self.map.clear();
        self.simple.clear();
        self.retired.clear();
        self.bytes = 0;
        self.retired_bytes = 0;
    }

    fn reclaim_retired(&mut self, now: Instant) {
        while let Some((retired_at, _)) = self.retired.front() {
            if now.saturating_duration_since(*retired_at) >= ICC_RETIRE_GRACE
                || self.retired_bytes > ICC_RETIRED_MAX_BYTES
            {
                if let Some((_, old_box)) = self.retired.pop_front() {
                    self.retired_bytes = self.retired_bytes.saturating_sub(old_box.len());
                }
            } else {
                break;
            }
        }
    }

    fn touch_key(&mut self, key: u64) {
        if let Some(pos) = self.order.iter().position(|&k| k == key) {
            self.order.remove(pos);
            self.order.push_back(key);
        }
    }

    fn retain_with_key(&mut self, profile: &[u8]) -> Option<(*const u8, usize, u64)> {
        let now = Instant::now();
        self.reclaim_retired(now);
        if profile.is_empty() || profile.len() > ICC_MAX_BYTES {
            return None;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        profile.hash(&mut hasher);
        let mut key = hasher.finish();
        if let Some(existing) = self.map.get(&key) {
            if existing.as_ref() == profile {
                let ptr = existing.as_ptr();
                let len = existing.len();
                self.touch_key(key);
                return Some((ptr, len, key));
            }
            let found = self
                .map
                .iter()
                .find(|(_, v)| v.as_ref() == profile)
                .map(|(&k, v)| (v.as_ptr(), v.len(), k));
            if let Some((ptr, len, existing_key)) = found {
                self.touch_key(existing_key);
                return Some((ptr, len, existing_key));
            }
            while self.map.contains_key(&key) {
                key = key.wrapping_add(1);
            }
        }
        while let Some(old) = (self.map.len() >= ICC_MAX_PROFILES
            || self.bytes.saturating_add(profile.len()) > ICC_MAX_BYTES)
            .then(|| self.order.pop_front())
            .flatten()
        {
            if let Some(old_box) = self.map.remove(&old) {
                let old_len = old_box.len();
                self.bytes -= old_len;
                self.retired_bytes = self.retired_bytes.saturating_add(old_len);
                self.retired.push_back((now, old_box));
                self.reclaim_retired(now);
            }
            self.simple.retain(|&(_, _, k)| k != old);
        }
        let mut buf = Vec::new();
        buf.try_reserve_exact(profile.len()).ok()?;
        buf.extend_from_slice(profile);
        let owned: Box<[u8]> = buf.into_boxed_slice();
        let ptr = owned.as_ptr();
        let len = owned.len();
        self.bytes += len;
        self.order.push_back(key);
        self.map.insert(key, owned);
        Some((ptr, len, key))
    }

    fn retain_profile(
        &mut self,
        profile: &JxlColorProfile,
        intensity_target: f32,
    ) -> Option<(*const u8, usize)> {
        match profile {
            JxlColorProfile::Icc(bytes) => {
                self.retain_with_key(bytes).map(|(ptr, len, _)| (ptr, len))
            }
            JxlColorProfile::Simple(enc) => {
                self.reclaim_retired(Instant::now());
                let target_bits = intensity_target.to_bits();
                if let Some(&(_, _, key)) = self
                    .simple
                    .iter()
                    .find(|(e, t, _)| e == enc && *t == target_bits)
                {
                    if let Some(existing) = self.map.get(&key) {
                        let ptr = existing.as_ptr();
                        let len = existing.len();
                        self.touch_key(key);
                        return Some((ptr, len));
                    }
                }
                #[cfg(feature = "upstream-jxl-icc-target")]
                let icc = profile.try_as_icc(intensity_target)?;
                #[cfg(not(feature = "upstream-jxl-icc-target"))]
                let icc = {
                    let _ = intensity_target;
                    profile.try_as_icc()?
                };
                let (ptr, len, key) = self.retain_with_key(&icc)?;
                if self.simple.len() >= ICC_MAX_PROFILES {
                    self.simple.remove(0);
                }
                self.simple.push((enc.clone(), target_bits, key));
                Some((ptr, len))
            }
        }
    }
}

fn default_threads() -> usize {
    static THREAD_COUNT: OnceLock<usize> = OnceLock::new();
    *THREAD_COUNT.get_or_init(|| {
        thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    })
}

#[derive(Clone, Copy)]
struct ErasedJob(*const (dyn Fn(usize) -> Result<(), JxlError> + Sync));

unsafe impl Send for ErasedJob {}
unsafe impl Sync for ErasedJob {}

struct PoolState {
    generation: u64,
    shutdown: bool,
    job: Option<ErasedJob>,
    num_tasks: usize,
    helper_limit: usize,
    pending_helpers: usize,
    first_error: Option<JxlError>,
    panic: Option<Box<dyn std::any::Any + Send + 'static>>,
}

struct SharedPool {
    state: Mutex<PoolState>,
    work_cv: Condvar,
    done_cv: Condvar,
    next_task: AtomicUsize,
    stop: AtomicBool,
    canceled: AtomicBool,
    cancel_token: usize,
}

/// Per-decode lazy worker pool implementing `JxlParallelRunner`.
///
/// Worker threads are spawned lazily up to `available_parallelism() - 1` and
/// reused across all `run_ordered` phases of a single `decode_static_raster`
/// call, then joined on `Drop` so no background threads outlive an FFI call or
/// block Windows `FreeLibrary` DLL unload.
struct DecodeRunner {
    max_threads: usize,
    shared: Arc<SharedPool>,
    workers: Vec<JoinHandle<()>>,
}

impl DecodeRunner {
    fn new(cancel: *mut c_void) -> Self {
        Self {
            max_threads: default_threads(),
            shared: Arc::new(SharedPool {
                state: Mutex::new(PoolState {
                    generation: 0,
                    shutdown: false,
                    job: None,
                    num_tasks: 0,
                    helper_limit: 0,
                    pending_helpers: 0,
                    first_error: None,
                    panic: None,
                }),
                work_cv: Condvar::new(),
                done_cv: Condvar::new(),
                next_task: AtomicUsize::new(0),
                stop: AtomicBool::new(false),
                canceled: AtomicBool::new(false),
                cancel_token: cancel as usize,
            }),
            workers: Vec::new(),
        }
    }

    fn was_canceled(&self) -> bool {
        self.shared.canceled.load(Ordering::Acquire)
    }

    fn run_serial(&self, num: usize, fun: &JxlParallelRunnerFun<'_>) -> Result<(), JxlError> {
        let cancel = self.shared.cancel_token as *mut c_void;
        for i in 0..num {
            if canceled(cancel) {
                self.shared.canceled.store(true, Ordering::Release);
                return Err(JxlError::IOError(std::io::Error::other("canceled")));
            }
            fun(i)?;
        }
        Ok(())
    }
}

fn run_tasks(shared: &SharedPool, job: ErasedJob, num_tasks: usize) -> Result<(), JxlError> {
    let cancel = shared.cancel_token as *mut c_void;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if canceled(cancel) {
            shared.canceled.store(true, Ordering::Release);
            shared.stop.store(true, Ordering::Release);
            return Err(JxlError::IOError(std::io::Error::other("canceled")));
        }
        let t = shared.next_task.fetch_add(1, Ordering::Relaxed);
        if t >= num_tasks {
            return Ok(());
        }
        // SAFETY: `job` points to the `fun` trait object alive on the caller's
        // stack while `JobFinishGuard` keeps `run_ordered` blocked until all
        // helpers finish and drop `job`.
        unsafe { (*job.0)(t)? };
    }
}

fn worker_loop(shared: Arc<SharedPool>, worker_id: usize, initial_gen: u64) {
    let mut seen_gen = initial_gen;
    loop {
        let (job, num_tasks) = {
            let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if st.shutdown {
                    return;
                }
                if st.generation != seen_gen {
                    seen_gen = st.generation;
                    if worker_id < st.helper_limit {
                        if let Some(job) = st.job {
                            break (job, st.num_tasks);
                        }
                    }
                }
                st = shared.work_cv.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };

        let res = catch_unwind(AssertUnwindSafe(|| run_tasks(&shared, job, num_tasks)));
        let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                shared.stop.store(true, Ordering::Release);
                if st.first_error.is_none() {
                    st.first_error = Some(e);
                }
            }
            Err(p) => {
                shared.stop.store(true, Ordering::Release);
                if st.panic.is_none() {
                    st.panic = Some(p);
                }
            }
        }
        st.pending_helpers -= 1;
        if st.pending_helpers == 0 {
            shared.done_cv.notify_one();
        }
    }
}

struct JobFinishGuard<'a>(&'a SharedPool);

impl Drop for JobFinishGuard<'_> {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::Release);
        let mut st = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        while st.pending_helpers > 0 {
            st = self.0.done_cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        st.job = None;
    }
}

impl JxlParallelRunner for DecodeRunner {
    fn run(&mut self, num: usize, fun: &JxlParallelRunnerFun<'_>) -> Result<(), JxlError> {
        self.run_ordered(num, None, fun)
    }

    fn num_threads(&self) -> usize {
        self.max_threads
    }

    fn run_ordered(
        &mut self,
        num: usize,
        max_threads: Option<usize>,
        fun: &JxlParallelRunnerFun<'_>,
    ) -> Result<(), JxlError> {
        let cancel = self.shared.cancel_token as *mut c_void;
        if canceled(cancel) {
            self.shared.canceled.store(true, Ordering::Release);
            return Err(JxlError::IOError(std::io::Error::other("canceled")));
        }

        let total_workers = max_threads
            .unwrap_or(usize::MAX)
            .min(self.max_threads)
            .min(num);
        if total_workers <= 1 {
            return self.run_serial(num, fun);
        }

        let mut helpers = total_workers - 1;
        if self.workers.len() < helpers {
            let current_gen = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .generation;
            for worker_id in self.workers.len()..helpers {
                let shared = Arc::clone(&self.shared);
                match thread::Builder::new().spawn(move || {
                    worker_loop(shared, worker_id, current_gen);
                }) {
                    Ok(handle) => self.workers.push(handle),
                    Err(_) => break,
                }
            }
            helpers = self.workers.len();
        }

        if helpers == 0 {
            return self.run_serial(num, fun);
        }

        self.shared.next_task.store(0, Ordering::Release);
        self.shared.stop.store(false, Ordering::Release);

        // SAFETY: `fun` stays alive on this stack frame until `guard` drops
        // and guarantees all helper threads have exited `run_tasks`.
        let erased = ErasedJob(unsafe {
            std::mem::transmute::<
                &JxlParallelRunnerFun<'_>,
                &'static (dyn Fn(usize) -> Result<(), JxlError> + Sync),
            >(fun) as *const _
        });

        {
            let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            st.job = Some(erased);
            st.num_tasks = num;
            st.helper_limit = helpers;
            st.pending_helpers = helpers;
            st.first_error = None;
            st.panic = None;
            st.generation = st.generation.wrapping_add(1);
        }
        self.shared.work_cv.notify_all();

        let guard = JobFinishGuard(&self.shared);
        let main_res = run_tasks(&self.shared, erased, num);
        if main_res.is_err() {
            self.shared.stop.store(true, Ordering::Release);
        }
        drop(guard);

        let (panic, first_error) = {
            let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            (st.panic.take(), st.first_error.take())
        };
        if let Some(p) = panic {
            std::panic::resume_unwind(p);
        }
        main_res?;
        if let Some(err) = first_error {
            return Err(err);
        }
        Ok(())
    }
}

impl Drop for DecodeRunner {
    fn drop(&mut self) {
        {
            let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            st.shutdown = true;
        }
        self.shared.work_cv.notify_all();
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

/// 64-byte aligned, OS demand-zero pixel buffer (`alloc_zeroed`).
///
/// Avoids a synchronous single-threaded `memset` over multi-GB frames, lets
/// worker threads fault pages in parallel during rendering, and eliminates the
/// secondary `Box<Vec<u8>>` allocation for `IGPixelBuffer::release_context`.
struct AlignedBuffer {
    ptr: NonNull<u8>,
    len: usize,
}

unsafe impl Send for AlignedBuffer {}
unsafe impl Sync for AlignedBuffer {}

impl AlignedBuffer {
    fn alloc_zeroed(len: usize) -> Option<Self> {
        if len == 0 || len > isize::MAX as usize {
            return None;
        }
        let layout = Layout::from_size_align(len, PIXEL_BUFFER_ALIGN).ok()?;
        let raw = unsafe { alloc_zeroed(layout) };
        NonNull::new(raw).map(|ptr| Self { ptr, len })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    fn as_output_buffer(
        &mut self,
        height: usize,
        bytes_per_row: usize,
    ) -> Result<JxlOutputBuffer<'_>, IGStatus> {
        if height == 0
            || bytes_per_row == 0
            || height
                .checked_mul(bytes_per_row)
                .is_none_or(|n| n > self.len)
        {
            return Err(DECODE_FAILED);
        }
        // SAFETY: `height * bytes_per_row <= self.len` is verified above and
        // `&mut self` grants exclusive mutable access for the returned lifetime.
        Ok(unsafe {
            JxlOutputBuffer::new_from_ptr(self.ptr.as_ptr(), height, bytes_per_row, bytes_per_row)
        })
    }

    fn into_pixel_buffer(self, width: usize, height: usize) -> Result<IGPixelBuffer, IGStatus> {
        if width == 0 || height == 0 {
            return Err(DECODE_FAILED);
        }
        let w = i32::try_from(width).map_err(|_| DECODE_FAILED)?;
        let h = i32::try_from(height).map_err(|_| DECODE_FAILED)?;
        let stride = width
            .checked_mul(4)
            .and_then(|s| i32::try_from(s).ok())
            .ok_or(DECODE_FAILED)?;
        let needed = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .ok_or(DECODE_FAILED)?;
        if needed > self.len {
            return Err(DECODE_FAILED);
        }
        let data = self.ptr.as_ptr();
        let release_context = self.len as *mut c_void;
        std::mem::forget(self);
        Ok(IGPixelBuffer {
            data,
            width: w,
            height: h,
            stride,
            pixel_format: PIXEL_BGRA8,
            release_context,
        })
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        if let Ok(layout) = Layout::from_size_align(self.len, PIXEL_BUFFER_ALIGN) {
            unsafe { dealloc(self.ptr.as_ptr(), layout) };
        }
    }
}

/// Checked-out `JxlDecoder` that resets and returns to `PluginState::decoders`
/// on drop so `BufferRecycler` buckets and scratch allocations stay warm.
struct PooledDecoder {
    decoder: Option<JxlDecoder>,
    recycle: bool,
}

impl PooledDecoder {
    fn checkout() -> Self {
        let reused = STATE
            .get()
            .and_then(|s| s.decoders.lock().ok())
            .and_then(|mut pool| pool.pop());
        let decoder = reused.unwrap_or_else(|| {
            let mut options = JxlDecoderOptions::default();
            options.sample_limit = Some(JXL_SAMPLE_LIMIT);
            options.alpha_mode = AlphaMode::Unpremultiplied;
            JxlDecoder::new(options)
        });
        Self {
            decoder: Some(decoder),
            recycle: true,
        }
    }

    fn set_recycle(&mut self, recycle: bool) {
        self.recycle = recycle;
    }
}

impl Deref for PooledDecoder {
    type Target = JxlDecoder;
    fn deref(&self) -> &Self::Target {
        self.decoder.as_ref().expect("decoder active")
    }
}

impl DerefMut for PooledDecoder {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.decoder.as_mut().expect("decoder active")
    }
}

impl Drop for PooledDecoder {
    fn drop(&mut self) {
        let Some(mut dec) = self.decoder.take() else {
            return;
        };
        if !self.recycle {
            return;
        }
        let Some(state) = STATE.get() else {
            return;
        };
        let Ok(mut pool) = state.decoders.lock() else {
            return;
        };
        if pool.len() < MAX_POOLED_DECODERS {
            dec.reset();
            pool.push(dec);
        }
    }
}

struct PluginState {
    plugin: Box<IGPluginApi>,
    codec: Box<IGCodecApi>,
    capability: Box<IGCodecCapability>,
    _strings: Vec<Vec<u16>>,
    _extensions: Box<[IGStringRef]>,
    icc: Mutex<IccStore>,
    decoders: Mutex<Vec<JxlDecoder>>,
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
        let mut strings: Vec<Vec<u16>> = [
            "Plugin_JxlCodec",
            "JPEG XL",
            env!("CARGO_PKG_VERSION"),
            "codec_jxl",
            ".jxl",
        ]
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
            decode_static_raster_scaled: Some(decode_static_raster_scaled),
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
            shutdown: Some(shutdown),
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
            decoders: Mutex::new(Vec::with_capacity(MAX_POOLED_DECODERS)),
        }
    }
}

unsafe extern "C" fn shutdown() {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Some(state) = STATE.get() else {
            return;
        };
        if let Ok(mut pool) = state.decoders.lock() {
            pool.clear();
            pool.shrink_to_fit();
        }
        if let Ok(mut icc) = state.icc.lock() {
            icc.clear();
        }
    }));
}

/// Returns the plugin's ABI function table for a compatible ImageGlass host.
///
/// # Safety
/// When non-null, `host_api` must point to a valid, initialized `IGHostApi`
/// whose `core` field (when non-null) points to a valid `IGHostCoreApi` struct
/// that remains live while plugin calls execute.
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
    if ext.data.is_null() || ext.length != 4 {
        return 0;
    }
    let s = unsafe { std::slice::from_raw_parts(ext.data, 4) };
    let matched = s[0] == b'.' as u16
        && (s[1] | 0x20) == b'j' as u16
        && (s[2] | 0x20) == b'x' as u16
        && (s[3] | 0x20) == b'l' as u16;
    matched as i32
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

fn bgra8_pixel_format(num_extra_channels: usize) -> JxlPixelFormat {
    JxlPixelFormat {
        color_type: JxlColorType::Bgra,
        color_data_format: Some(JxlDataFormat::U8 { bit_depth: 8 }),
        extra_channel_format: vec![None; num_extra_channels],
    }
}

fn validate_basic_info(basic: &JxlBasicInfo) -> Result<(usize, usize, bool, usize), IGStatus> {
    let (width, height) = basic.size;
    if width == 0
        || height == 0
        || width
            .checked_mul(height)
            .is_none_or(|n| n > MAX_IMAGE_PIXELS)
        || width
            .checked_mul(4)
            .and_then(|s| i32::try_from(s).ok())
            .is_none()
        || height > i32::MAX as usize
    {
        log_host(
            LOG_ERROR,
            &format!("jxl_codec: unsupported or overflowing image dimensions {width}x{height}"),
        );
        return Err(DECODE_FAILED);
    }
    if basic.animation.is_some() {
        log_host(
            LOG_WARN,
            "jxl_codec: animated JXL images are not supported by static raster codec",
        );
        return Err(UNSUPPORTED);
    }
    if basic
        .extra_channels
        .iter()
        .any(|c| matches!(c.ec_type, ExtraChannel::Black))
    {
        log_host(
            LOG_WARN,
            "jxl_codec: CMYK JXL images with Black extra channel are not supported",
        );
        return Err(UNSUPPORTED);
    }
    let alpha = basic
        .extra_channels
        .iter()
        .any(|c| matches!(c.ec_type, ExtraChannel::Alpha));
    Ok((width, height, alpha, basic.extra_channels.len()))
}

fn is_cmyk_decoder(decoder: &JxlDecoder) -> bool {
    decoder
        .output_color_profile()
        .is_some_and(|profile| profile.is_cmyk())
}

fn map_jxl_err(err: JxlError, input: &FileInput, cancel: *mut c_void) -> IGStatus {
    if input.reader.was_canceled || canceled(cancel) {
        return CANCELED;
    }
    if matches!(
        &err,
        JxlError::OutOfMemory(_) | JxlError::ImageOutOfMemory(_, _)
    ) {
        log_host(
            LOG_ERROR,
            &format!("jxl_codec: decoder out of memory: {err}"),
        );
        return OUT_OF_MEMORY;
    }
    if matches!(
        &err,
        JxlError::IOError(io_err) if io_err.kind() == std::io::ErrorKind::OutOfMemory
    ) {
        log_host(
            LOG_ERROR,
            &format!("jxl_codec: read buffer out of memory: {err}"),
        );
        return OUT_OF_MEMORY;
    }
    if input.reader.io_err || matches!(&err, JxlError::IOError(_)) {
        log_host(LOG_ERROR, &format!("jxl_codec: stream I/O error: {err}"));
        return IO_ERROR;
    }
    log_host(LOG_ERROR, &format!("jxl_codec: decode failed: {err}"));
    DECODE_FAILED
}

fn read_until_basic_info(
    decoder: &mut JxlDecoder,
    input: &mut FileInput,
    cancel: *mut c_void,
) -> Result<(), IGStatus> {
    loop {
        if canceled(cancel) {
            return Err(CANCELED);
        }
        match decoder.process(input, None, None) {
            Ok(Event::BasicInfo) => return Ok(()),
            Ok(Event::NeedMoreInput { .. }) => {
                if input.is_exhausted() {
                    log_host(
                        LOG_ERROR,
                        "jxl_codec: unexpected EOF while parsing JXL header",
                    );
                    return Err(DECODE_FAILED);
                }
            }
            Ok(Event::Complete) => return Ok(()),
            Ok(_) => {}
            Err(err) => return Err(map_jxl_err(err, input, cancel)),
        }
    }
}

fn frame_fits_available_memory(
    bytes: usize,
    extra_margin_bytes: usize,
    avail_bytes: usize,
) -> bool {
    // A 32000x32000 frame alone is ~4 GB, and jxl-rs, compressed sections, plus
    // the host's own copy need more on top. Refuse into OutOfMemory rather than
    // paging the host.
    let required = bytes
        .saturating_add(bytes / 2)
        .saturating_add(extra_margin_bytes)
        .saturating_add(64 * 1024 * 1024);
    required <= avail_bytes
}

fn allocate_checked_frame(
    width: usize,
    height: usize,
    extra_margin_bytes: usize,
) -> Result<AlignedBuffer, IGStatus> {
    let bytes = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(4))
        .filter(|&n| n <= MAX_OUTPUT_BYTES && n <= isize::MAX as usize)
        .ok_or_else(|| {
            log_host(
                LOG_ERROR,
                &format!("jxl_codec: output frame size overflow for {width}x{height}"),
            );
            DECODE_FAILED
        })?;
    let avail = available_physical_bytes();
    if !frame_fits_available_memory(bytes, extra_margin_bytes, avail) {
        log_host(
            LOG_ERROR,
            &format!(
                "jxl_codec: insufficient physical memory for {width}x{height} ({bytes} bytes + {extra_margin_bytes} margin, available {avail} bytes)"
            ),
        );
        return Err(OUT_OF_MEMORY);
    }
    AlignedBuffer::alloc_zeroed(bytes).ok_or_else(|| {
        log_host(
            LOG_ERROR,
            &format!(
                "jxl_codec: pixel buffer allocation failed for {width}x{height} ({bytes} bytes)"
            ),
        );
        OUT_OF_MEMORY
    })
}

#[derive(Clone, Copy)]
struct FrameSpec {
    width: usize,
    height: usize,
    extra_channels: usize,
}

fn decode_full_frame(
    decoder: &mut JxlDecoder,
    input: &mut FileInput,
    pixels: &mut AlignedBuffer,
    spec: FrameSpec,
    runner: &mut DecodeRunner,
    cancel: *mut c_void,
) -> Result<(), IGStatus> {
    input.set_decode_mode();
    if decoder
        .set_pixel_format(bgra8_pixel_format(spec.extra_channels))
        .is_err()
    {
        log_host(
            LOG_ERROR,
            "jxl_codec: failed to configure BGRA8 pixel format",
        );
        return Err(DECODE_FAILED);
    }
    let bytes_per_row = spec.width.checked_mul(4).ok_or(DECODE_FAILED)?;
    let mut out_buf = [pixels.as_output_buffer(spec.height, bytes_per_row)?];
    let mut frame_complete = false;
    loop {
        if canceled(cancel) {
            return Err(CANCELED);
        }
        match decoder.process(input, Some(&mut out_buf), Some(runner)) {
            Ok(Event::FrameHeader) => {}
            Ok(Event::FrameComplete { .. }) => {
                frame_complete = true;
                break;
            }
            Ok(Event::Complete) => break,
            Ok(Event::NeedMoreInput { .. }) => {
                if input.is_exhausted() {
                    log_host(
                        LOG_ERROR,
                        "jxl_codec: unexpected EOF while decoding JXL frame",
                    );
                    return Err(DECODE_FAILED);
                }
            }
            Ok(Event::BasicInfo) => {
                log_host(
                    LOG_ERROR,
                    "jxl_codec: unexpected duplicate BasicInfo event during frame decode",
                );
                return Err(DECODE_FAILED);
            }
            Err(err) => {
                if runner.was_canceled() {
                    return Err(CANCELED);
                }
                return Err(map_jxl_err(err, input, cancel));
            }
        }
    }
    if canceled(cancel) {
        return Err(CANCELED);
    }
    if frame_complete {
        Ok(())
    } else {
        log_host(LOG_ERROR, "jxl_codec: JXL stream completed without a frame");
        Err(DECODE_FAILED)
    }
}

fn fit_within_box(src_w: usize, src_h: usize, max_w: usize, max_h: usize) -> (usize, usize) {
    if src_w <= max_w && src_h <= max_h {
        return (src_w, src_h);
    }
    let lhs = (src_w as u128) * (max_h as u128);
    let rhs = (max_w as u128) * (src_h as u128);
    if lhs >= rhs {
        let dst_w = max_w;
        let dst_h = (((src_h as u128) * (max_w as u128) + (src_w as u128) / 2) / (src_w as u128))
            .clamp(1, max_h as u128) as usize;
        (dst_w, dst_h)
    } else {
        let dst_h = max_h;
        let dst_w = (((src_w as u128) * (max_h as u128) + (src_h as u128) / 2) / (src_h as u128))
            .clamp(1, max_w as u128) as usize;
        (dst_w, dst_h)
    }
}

#[derive(Clone, Copy)]
struct SendMutPtr(*mut u8);

unsafe impl Send for SendMutPtr {}
unsafe impl Sync for SendMutPtr {}

impl SendMutPtr {
    #[inline(always)]
    unsafe fn add(self, offset: usize) -> *mut u8 {
        unsafe { self.0.add(offset) }
    }
}

struct ImagePlane<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
}

struct ImagePlaneMut<'a> {
    data: &'a mut [u8],
    width: usize,
    height: usize,
}

fn downscale_bgra8(
    src: ImagePlane<'_>,
    dst: ImagePlaneMut<'_>,
    has_alpha: bool,
    runner: &mut DecodeRunner,
    cancel: *mut c_void,
) -> Result<(), IGStatus> {
    let src_w = src.width;
    let src_h = src.height;
    let dst_w = dst.width;
    let dst_h = dst.height;
    let Some(src_stride) = src_w.checked_mul(4) else {
        return Err(DECODE_FAILED);
    };
    let Some(dst_stride) = dst_w.checked_mul(4) else {
        return Err(DECODE_FAILED);
    };
    let Some(src_needed) = src_stride.checked_mul(src_h) else {
        return Err(DECODE_FAILED);
    };
    let Some(dst_needed) = dst_stride.checked_mul(dst_h) else {
        return Err(DECODE_FAILED);
    };
    if src_w == 0
        || src_h == 0
        || dst_w == 0
        || dst_h == 0
        || src.data.len() < src_needed
        || dst.data.len() < dst_needed
    {
        return Err(DECODE_FAILED);
    }
    let mut x_spans = Vec::new();
    x_spans
        .try_reserve_exact(dst_w)
        .map_err(|_| OUT_OF_MEMORY)?;
    for dx in 0..dst_w {
        let x0 = ((dx as u128 * src_w as u128) / dst_w as u128) as usize;
        let x1 = ((((dx + 1) as u128 * src_w as u128) / dst_w as u128) as usize)
            .max(x0 + 1)
            .min(src_w);
        x_spans.push((x0, x1));
    }
    let src_data = src.data;
    let dst_base = SendMutPtr(dst.data.as_mut_ptr());
    let max_threads = dst_h.div_ceil(16).max(1);
    let res = runner.run_ordered(dst_h, Some(max_threads), &|dy| {
        let y0 = ((dy as u128 * src_h as u128) / dst_h as u128) as usize;
        let y1 = ((((dy + 1) as u128 * src_h as u128) / dst_h as u128) as usize)
            .max(y0 + 1)
            .min(src_h);
        // SAFETY: `dst.data.len() >= dst_stride * dst_h` is verified above, and each
        // `dy` writes strictly to its own disjoint row `dy * dst_stride`.
        let dst_row =
            unsafe { std::slice::from_raw_parts_mut(dst_base.add(dy * dst_stride), dst_stride) };
        if !has_alpha {
            for (dx, &(x0, x1)) in x_spans.iter().enumerate() {
                let mut sum_b = 0u64;
                let mut sum_g = 0u64;
                let mut sum_r = 0u64;
                for sy in y0..y1 {
                    let row = &src_data[sy * src_stride..(sy + 1) * src_stride];
                    for sx in x0..x1 {
                        let off = sx * 4;
                        sum_b += row[off] as u64;
                        sum_g += row[off + 1] as u64;
                        sum_r += row[off + 2] as u64;
                    }
                }
                let count = ((x1 - x0) as u64) * ((y1 - y0) as u64);
                let half = count / 2;
                let out = dx * 4;
                dst_row[out] = ((sum_b + half) / count) as u8;
                dst_row[out + 1] = ((sum_g + half) / count) as u8;
                dst_row[out + 2] = ((sum_r + half) / count) as u8;
                dst_row[out + 3] = 255;
            }
        } else {
            for (dx, &(x0, x1)) in x_spans.iter().enumerate() {
                let mut sum_ba = 0u64;
                let mut sum_ga = 0u64;
                let mut sum_ra = 0u64;
                let mut sum_a = 0u64;
                for sy in y0..y1 {
                    let row = &src_data[sy * src_stride..(sy + 1) * src_stride];
                    for sx in x0..x1 {
                        let off = sx * 4;
                        let a = row[off + 3] as u64;
                        sum_ba += (row[off] as u64) * a;
                        sum_ga += (row[off + 1] as u64) * a;
                        sum_ra += (row[off + 2] as u64) * a;
                        sum_a += a;
                    }
                }
                let count = ((x1 - x0) as u64) * ((y1 - y0) as u64);
                let out = dx * 4;
                let out_a = ((sum_a + count / 2) / count) as u8;
                if sum_a > 0 {
                    let half_a = sum_a / 2;
                    dst_row[out] = ((sum_ba + half_a) / sum_a).min(255) as u8;
                    dst_row[out + 1] = ((sum_ga + half_a) / sum_a).min(255) as u8;
                    dst_row[out + 2] = ((sum_ra + half_a) / sum_a).min(255) as u8;
                } else {
                    dst_row[out] = 0;
                    dst_row[out + 1] = 0;
                    dst_row[out + 2] = 0;
                }
                dst_row[out + 3] = out_a;
            }
        }
        Ok(())
    });
    match res {
        Ok(()) => Ok(()),
        Err(_) if runner.was_canceled() || canceled(cancel) => Err(CANCELED),
        Err(_) => {
            log_host(LOG_ERROR, "jxl_codec: parallel thumbnail downscale failed");
            Err(DECODE_FAILED)
        }
    }
}

unsafe extern "C" fn load_metadata(
    path: IGStringRef,
    out: *mut IGImageInfo,
    cancel: *mut c_void,
) -> i32 {
    boundary(|| {
        if !out.is_null() {
            unsafe {
                *out = IGImageInfo::default();
            }
        }
        if path.data.is_null() || path.length < 0 || out.is_null() {
            return INVALID_ARG;
        }
        if canceled(cancel) {
            return CANCELED;
        }
        let path = match unsafe { string_path(path) } {
            Ok(p) => p,
            Err(s) => return s,
        };
        let mut input = match FileInput::open(&path, METADATA_READ_BUF, cancel) {
            Ok(f) => f,
            Err(e) => {
                log_host(
                    LOG_ERROR,
                    &format!("jxl_codec: failed to open {}: {e}", path.display()),
                );
                return IO_ERROR;
            }
        };
        let file_size = i64::try_from(input.length).unwrap_or(-1);

        let mut decoder = PooledDecoder::checkout();
        if let Err(status) = read_until_basic_info(&mut decoder, &mut input, cancel) {
            return status;
        }

        let Some(basic) = decoder.basic_info() else {
            log_host(
                LOG_ERROR,
                "jxl_codec: missing basic info after header parse",
            );
            return DECODE_FAILED;
        };
        let intensity_target = basic.tone_mapping.intensity_target;
        let (width, height, alpha, extra_channels) = match validate_basic_info(basic) {
            Ok(info) => info,
            Err(status) => return status,
        };

        // Ensure XYB output color profile resolves for BGRA8 (sRGB) before querying.
        if decoder
            .set_pixel_format(bgra8_pixel_format(extra_channels))
            .is_err()
        {
            log_host(
                LOG_ERROR,
                "jxl_codec: failed to configure BGRA8 pixel format",
            );
            return DECODE_FAILED;
        }

        // jxl-rs renders upright by default (`adjust_orientation = true`, and the
        // pipeline rotates the pixels), so `basic.size` is already the oriented size
        // and the host must not rotate again — always report Identity.
        let (icc_ptr, icc_len) = match decoder.output_color_profile() {
            Some(profile) => {
                if profile.is_cmyk() {
                    log_host(
                        LOG_WARN,
                        "jxl_codec: CMYK JXL color profiles are not supported",
                    );
                    return UNSUPPORTED;
                }
                let Ok(mut store) = STATE.get().expect("plugin initialized").icc.lock() else {
                    return INTERNAL;
                };
                store
                    .retain_profile(profile, intensity_target)
                    .unwrap_or((std::ptr::null(), 0))
            }
            None => (std::ptr::null(), 0),
        };

        unsafe {
            *out = IGImageInfo {
                width: width as i32,
                height: height as i32,
                pixel_format: PIXEL_BGRA8,
                has_alpha: alpha as i32,
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

struct OpenedDecode {
    input: FileInput,
    decoder: PooledDecoder,
    width: usize,
    height: usize,
    alpha: bool,
    extra_channels: usize,
    preview_oriented: Option<(usize, usize)>,
}

/// # Safety
/// `path` must satisfy the preconditions of `string_path`.
unsafe fn open_decode_session(
    path: IGStringRef,
    cancel: *mut c_void,
) -> Result<OpenedDecode, IGStatus> {
    let path = unsafe { string_path(path)? };
    let mut input = FileInput::open(&path, METADATA_READ_BUF, cancel).map_err(|e| {
        log_host(
            LOG_ERROR,
            &format!("jxl_codec: failed to open {}: {e}", path.display()),
        );
        IO_ERROR
    })?;
    let mut decoder = PooledDecoder::checkout();
    read_until_basic_info(&mut decoder, &mut input, cancel)?;

    let Some(basic) = decoder.basic_info() else {
        log_host(
            LOG_ERROR,
            "jxl_codec: missing basic info after header parse",
        );
        return Err(DECODE_FAILED);
    };
    let (width, height, alpha, extra_channels) = validate_basic_info(basic)?;
    let preview_oriented = basic
        .preview_size
        .map(|ps| basic.orientation.map_size(ps))
        .filter(|&(pw, ph)| pw > 0 && ph > 0);
    if decoder
        .set_pixel_format(bgra8_pixel_format(extra_channels))
        .is_err()
    {
        log_host(
            LOG_ERROR,
            "jxl_codec: failed to configure BGRA8 pixel format",
        );
        return Err(DECODE_FAILED);
    }
    if is_cmyk_decoder(&decoder) {
        log_host(
            LOG_WARN,
            "jxl_codec: CMYK JXL color profiles are not supported",
        );
        return Err(UNSUPPORTED);
    }
    decoder.set_recycle(width.saturating_mul(height) <= DECODER_POOL_MAX_PIXELS);
    Ok(OpenedDecode {
        input,
        decoder,
        width,
        height,
        alpha,
        extra_channels,
        preview_oriented,
    })
}

unsafe extern "C" fn decode_static_raster(
    path: IGStringRef,
    frame: i32,
    out: *mut IGPixelBuffer,
    cancel: *mut c_void,
) -> i32 {
    boundary(|| {
        if !out.is_null() {
            unsafe {
                *out = IGPixelBuffer::default();
            }
        }
        if path.data.is_null() || path.length < 0 || out.is_null() || frame != 0 {
            return INVALID_ARG;
        }
        if canceled(cancel) {
            return CANCELED;
        }
        let mut session = match unsafe { open_decode_session(path, cancel) } {
            Ok(s) => s,
            Err(status) => return status,
        };
        let mut pixels = match allocate_checked_frame(
            session.width,
            session.height,
            session.input.compressed_margin(),
        ) {
            Ok(b) => b,
            Err(status) => return status,
        };

        let mut runner = DecodeRunner::new(cancel);
        if let Err(status) = decode_full_frame(
            &mut session.decoder,
            &mut session.input,
            &mut pixels,
            FrameSpec {
                width: session.width,
                height: session.height,
                extra_channels: session.extra_channels,
            },
            &mut runner,
            cancel,
        ) {
            return status;
        }

        let buf = match pixels.into_pixel_buffer(session.width, session.height) {
            Ok(b) => b,
            Err(status) => return status,
        };
        unsafe {
            *out = buf;
        }
        OK
    }) as i32
}

struct PreviewTarget {
    pw: usize,
    ph: usize,
    dst_w: usize,
    dst_h: usize,
    max_w: usize,
    max_h: usize,
    alpha: bool,
    extra_channels: usize,
}

fn is_terminal_preview_err(status: IGStatus) -> bool {
    matches!(status, CANCELED | OUT_OF_MEMORY | IO_ERROR | UNSUPPORTED)
}

fn try_decode_preview(
    input: &mut FileInput,
    target: PreviewTarget,
    cancel: *mut c_void,
) -> Result<Option<IGPixelBuffer>, IGStatus> {
    if let Err(e) = input.rewind_to_start() {
        log_host(
            LOG_ERROR,
            &format!("jxl_codec: failed to rewind stream for preview: {e}"),
        );
        return Err(IO_ERROR);
    }
    let mut preview_opts = JxlDecoderOptions::default();
    preview_opts.sample_limit = Some(JXL_SAMPLE_LIMIT);
    preview_opts.alpha_mode = AlphaMode::Unpremultiplied;
    preview_opts.skip_preview = false;
    let mut preview_dec = JxlDecoder::new(preview_opts);
    if let Err(status) = read_until_basic_info(&mut preview_dec, input, cancel) {
        return if is_terminal_preview_err(status) {
            Err(status)
        } else {
            Ok(None)
        };
    }
    let mut prev_buf = match allocate_checked_frame(target.pw, target.ph, input.compressed_margin())
    {
        Ok(buf) => buf,
        Err(status) => {
            return if is_terminal_preview_err(status) {
                Err(status)
            } else {
                Ok(None)
            };
        }
    };
    let mut runner = DecodeRunner::new(cancel);
    if let Err(status) = decode_full_frame(
        &mut preview_dec,
        input,
        &mut prev_buf,
        FrameSpec {
            width: target.pw,
            height: target.ph,
            extra_channels: target.extra_channels,
        },
        &mut runner,
        cancel,
    ) {
        return if is_terminal_preview_err(status) {
            Err(status)
        } else {
            Ok(None)
        };
    }

    let same_aspect = target.pw <= target.max_w
        && target.ph <= target.max_h
        && (target.pw * target.dst_h).abs_diff(target.ph * target.dst_w)
            <= target.pw.max(target.ph);
    if same_aspect {
        return match prev_buf.into_pixel_buffer(target.pw, target.ph) {
            Ok(buf) => Ok(Some(buf)),
            Err(status) if is_terminal_preview_err(status) => Err(status),
            Err(_) => Ok(None),
        };
    }

    let Some(thumb_bytes) = target
        .dst_w
        .checked_mul(target.dst_h)
        .and_then(|n| n.checked_mul(4))
        .filter(|&n| n <= MAX_OUTPUT_BYTES && n <= isize::MAX as usize)
    else {
        return Ok(None);
    };
    let Some(mut thumb) = AlignedBuffer::alloc_zeroed(thumb_bytes) else {
        log_host(
            LOG_ERROR,
            &format!(
                "jxl_codec: preview thumbnail buffer allocation failed ({}x{})",
                target.dst_w, target.dst_h
            ),
        );
        return Err(OUT_OF_MEMORY);
    };
    if let Err(status) = downscale_bgra8(
        ImagePlane {
            data: prev_buf.as_slice(),
            width: target.pw,
            height: target.ph,
        },
        ImagePlaneMut {
            data: thumb.as_mut_slice(),
            width: target.dst_w,
            height: target.dst_h,
        },
        target.alpha,
        &mut runner,
        cancel,
    ) {
        return if is_terminal_preview_err(status) {
            Err(status)
        } else {
            Ok(None)
        };
    }
    match thumb.into_pixel_buffer(target.dst_w, target.dst_h) {
        Ok(buf) => Ok(Some(buf)),
        Err(status) if is_terminal_preview_err(status) => Err(status),
        Err(_) => Ok(None),
    }
}

unsafe extern "C" fn decode_static_raster_scaled(
    path: IGStringRef,
    frame: i32,
    max_width: i32,
    max_height: i32,
    out: *mut IGPixelBuffer,
    cancel: *mut c_void,
) -> i32 {
    boundary(|| {
        if !out.is_null() {
            unsafe {
                *out = IGPixelBuffer::default();
            }
        }
        if path.data.is_null()
            || path.length < 0
            || out.is_null()
            || frame != 0
            || max_width <= 0
            || max_height <= 0
        {
            return INVALID_ARG;
        }
        if canceled(cancel) {
            return CANCELED;
        }
        let OpenedDecode {
            mut input,
            mut decoder,
            width,
            height,
            alpha,
            extra_channels,
            preview_oriented,
        } = match unsafe { open_decode_session(path, cancel) } {
            Ok(s) => s,
            Err(status) => return status,
        };

        let max_w = max_width as usize;
        let max_h = max_height as usize;
        let (dst_w, dst_h) = fit_within_box(width, height, max_w, max_h);

        // Unscaled fast path: source already fits inside max_width x max_height.
        if dst_w == width && dst_h == height {
            let mut pixels = match allocate_checked_frame(width, height, input.compressed_margin())
            {
                Ok(b) => b,
                Err(status) => return status,
            };
            let mut runner = DecodeRunner::new(cancel);
            if let Err(status) = decode_full_frame(
                &mut decoder,
                &mut input,
                &mut pixels,
                FrameSpec {
                    width,
                    height,
                    extra_channels,
                },
                &mut runner,
                cancel,
            ) {
                return status;
            }
            let buf = match pixels.into_pixel_buffer(width, height) {
                Ok(b) => b,
                Err(status) => return status,
            };
            unsafe {
                *out = buf;
            }
            return OK;
        }

        // Embedded preview fast path: decode only the preview frame if it covers
        // the requested box and is smaller than the full-resolution image.
        if let Some((pw, ph)) = preview_oriented {
            if pw >= dst_w && ph >= dst_h && pw.saturating_mul(ph) < width.saturating_mul(height) {
                match try_decode_preview(
                    &mut input,
                    PreviewTarget {
                        pw,
                        ph,
                        dst_w,
                        dst_h,
                        max_w,
                        max_h,
                        alpha,
                        extra_channels,
                    },
                    cancel,
                ) {
                    Ok(Some(buf)) => {
                        unsafe {
                            *out = buf;
                        }
                        return OK;
                    }
                    Err(status) => return status,
                    Ok(None) => {
                        // Rewind for normal full-image decode if recoverable preview fallback occurred.
                        if let Err(e) = input.rewind_to_start() {
                            log_host(
                                LOG_ERROR,
                                &format!("jxl_codec: rewind after preview fallback failed: {e}"),
                            );
                            return IO_ERROR;
                        }
                        drop(decoder);
                        decoder = PooledDecoder::checkout();
                        decoder
                            .set_recycle(width.saturating_mul(height) <= DECODER_POOL_MAX_PIXELS);
                        if let Err(status) = read_until_basic_info(&mut decoder, &mut input, cancel)
                        {
                            return status;
                        }
                    }
                }
            }
        }

        let thumb_bytes = match dst_w.checked_mul(dst_h).and_then(|n| n.checked_mul(4)) {
            Some(n) if n <= MAX_OUTPUT_BYTES && n <= isize::MAX as usize => n,
            _ => return DECODE_FAILED,
        };
        let mut full_pixels = match allocate_checked_frame(
            width,
            height,
            thumb_bytes.saturating_add(input.compressed_margin()),
        ) {
            Ok(b) => b,
            Err(status) => return status,
        };
        let Some(mut thumb_pixels) = AlignedBuffer::alloc_zeroed(thumb_bytes) else {
            log_host(
                LOG_ERROR,
                &format!("jxl_codec: thumbnail buffer allocation failed ({dst_w}x{dst_h})"),
            );
            return OUT_OF_MEMORY;
        };

        let mut runner = DecodeRunner::new(cancel);
        // Decode to frame completion before downscaling; `flush_pixels() == Ok(true)`
        // only indicates partial dirty-region updates rather than whole-frame coverage.
        if let Err(status) = decode_full_frame(
            &mut decoder,
            &mut input,
            &mut full_pixels,
            FrameSpec {
                width,
                height,
                extra_channels,
            },
            &mut runner,
            cancel,
        ) {
            return status;
        }

        if let Err(status) = downscale_bgra8(
            ImagePlane {
                data: full_pixels.as_slice(),
                width,
                height,
            },
            ImagePlaneMut {
                data: thumb_pixels.as_mut_slice(),
                width: dst_w,
                height: dst_h,
            },
            alpha,
            &mut runner,
            cancel,
        ) {
            return status;
        }
        drop(full_pixels);

        let buf = match thumb_pixels.into_pixel_buffer(dst_w, dst_h) {
            Ok(b) => b,
            Err(status) => return status,
        };
        unsafe {
            *out = buf;
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
        let len = b.release_context as usize;
        if !b.data.is_null() && len > 0 {
            if let Ok(layout) = Layout::from_size_align(len, PIXEL_BUFFER_ALIGN) {
                unsafe { dealloc(b.data, layout) };
            }
        }
        *b = IGPixelBuffer::default();
    }));
}

/// # Safety
/// `s.data` must be either null or valid for reads of `s.length` UTF-16 code
/// units when `s.length >= 0`.
#[cfg(target_os = "windows")]
unsafe fn string_path(s: IGStringRef) -> Result<std::path::PathBuf, IGStatus> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    if s.data.is_null() || s.length < 0 {
        return Err(INVALID_ARG);
    }
    let units = unsafe { std::slice::from_raw_parts(s.data, s.length as usize) };
    Ok(std::path::PathBuf::from(OsString::from_wide(units)))
}

/// # Safety
/// `s.data` must be either null or valid for reads of `s.length` UTF-16 code
/// units when `s.length >= 0`.
#[cfg(not(target_os = "windows"))]
unsafe fn string_path(s: IGStringRef) -> Result<std::path::PathBuf, IGStatus> {
    if s.data.is_null() || s.length < 0 {
        return Err(INVALID_ARG);
    }
    let units = unsafe { std::slice::from_raw_parts(s.data, s.length as usize) };
    String::from_utf16(units)
        .map(std::path::PathBuf::from)
        .map_err(|_| INVALID_ARG)
}

fn log_host(level: i32, msg: &str) {
    let core = HOST_CORE.load(Ordering::Acquire);
    if core.is_null() {
        return;
    }
    let log_fn = unsafe { (*core).log };
    if let Some(f) = log_fn {
        let wide: Vec<u16> = msg.encode_utf16().collect();
        let s = IGStringRef {
            data: wide.as_ptr(),
            length: wide.len().min(i32::MAX as usize) as i32,
        };
        unsafe { f(level, s) };
    }
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn boundary(f: impl FnOnce() -> IGStatus) -> IGStatus {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(status) => status,
        Err(payload) => {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let msg = panic_payload_message(payload);
                log_host(
                    LOG_ERROR,
                    &format!("jxl_codec: panic caught at FFI boundary: {msg}"),
                );
            }));
            INTERNAL
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jxl::api::{JxlAnimation, JxlBitDepth, JxlExtraChannel, Orientation, ToneMapping};
    use std::sync::atomic::AtomicI32;

    static HOST_CORE_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct HostCoreGuard<'a> {
        _lock: std::sync::MutexGuard<'a, ()>,
        prev: *mut IGHostCoreApi,
    }

    impl<'a> HostCoreGuard<'a> {
        fn install(core: *mut IGHostCoreApi) -> Self {
            let lock = HOST_CORE_TEST_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev = HOST_CORE.swap(core, Ordering::AcqRel);
            Self { _lock: lock, prev }
        }
    }

    impl Drop for HostCoreGuard<'_> {
        fn drop(&mut self) {
            HOST_CORE.store(self.prev, Ordering::Release);
        }
    }

    fn make_basic_info(
        width: usize,
        height: usize,
        extra_channels: Vec<JxlExtraChannel>,
        animation: Option<JxlAnimation>,
    ) -> JxlBasicInfo {
        JxlBasicInfo {
            size: (width, height),
            bit_depth: JxlBitDepth::Int { bits_per_sample: 8 },
            orientation: Orientation::Identity,
            extra_channels,
            animation,
            uses_original_profile: true,
            tone_mapping: ToneMapping {
                intensity_target: 255.0,
                min_nits: 0.0,
                relative_to_max_display: false,
                linear_below: 0.0,
            },
            preview_size: None,
        }
    }

    #[test]
    fn validate_basic_info_stride_and_limits() {
        let max_w = (i32::MAX as usize) / 4;
        assert_eq!(
            validate_basic_info(&make_basic_info(max_w, 1, Vec::new(), None)),
            Ok((max_w, 1, false, 0))
        );
        assert_eq!(
            validate_basic_info(&make_basic_info(max_w + 1, 1, Vec::new(), None)),
            Err(DECODE_FAILED)
        );
        assert_eq!(
            validate_basic_info(&make_basic_info(600_000_000, 1, Vec::new(), None)),
            Err(DECODE_FAILED)
        );
        assert_eq!(
            validate_basic_info(&make_basic_info(0, 10, Vec::new(), None)),
            Err(DECODE_FAILED)
        );
        assert_eq!(
            validate_basic_info(&make_basic_info(10, 0, Vec::new(), None)),
            Err(DECODE_FAILED)
        );
        assert_eq!(
            validate_basic_info(&make_basic_info(32_000, 32_000, Vec::new(), None)),
            Ok((32_000, 32_000, false, 0))
        );
        assert_eq!(
            validate_basic_info(&make_basic_info(32_001, 32_000, Vec::new(), None)),
            Err(DECODE_FAILED)
        );
        let anim = Some(JxlAnimation {
            tps_numerator: 1,
            tps_denominator: 1,
            num_loops: 0,
            have_timecodes: false,
        });
        assert_eq!(
            validate_basic_info(&make_basic_info(16, 16, Vec::new(), anim)),
            Err(UNSUPPORTED)
        );
        assert_eq!(
            validate_basic_info(&make_basic_info(
                16,
                16,
                vec![JxlExtraChannel {
                    ec_type: ExtraChannel::Black,
                    alpha_associated: false,
                }],
                None,
            )),
            Err(UNSUPPORTED)
        );
        assert_eq!(
            validate_basic_info(&make_basic_info(
                16,
                16,
                vec![JxlExtraChannel {
                    ec_type: ExtraChannel::Alpha,
                    alpha_associated: true,
                }],
                None,
            )),
            Ok((16, 16, true, 1))
        );
    }

    #[test]
    fn cmyk_profile_detection() {
        let mut cmyk_icc = vec![0u8; 32];
        cmyk_icc[16..20].copy_from_slice(b"CMYK");
        assert!(JxlColorProfile::Icc(cmyk_icc).is_cmyk());

        let mut rgb_icc = vec![0u8; 32];
        rgb_icc[16..20].copy_from_slice(b"RGB ");
        assert!(!JxlColorProfile::Icc(rgb_icc).is_cmyk());
    }

    #[test]
    fn icc_store_enforces_size_and_count_caps_and_grace_window() {
        let mut store = IccStore::default();
        assert!(store.retain_with_key(&[]).is_none());

        let exact = vec![0x42u8; ICC_MAX_BYTES];
        let (ptr, len, _) = store.retain_with_key(&exact).expect("exact cap accepted");
        assert_eq!(len, ICC_MAX_BYTES);
        assert_eq!(store.bytes, ICC_MAX_BYTES);
        assert_eq!(
            unsafe { std::slice::from_raw_parts(ptr, len) },
            exact.as_slice()
        );

        let over = vec![0x99u8; ICC_MAX_BYTES + 1];
        assert!(store.retain_with_key(&over).is_none());
        assert_eq!(store.bytes, ICC_MAX_BYTES);
        assert_eq!(store.map.len(), 1);

        let mut empty_store = IccStore::default();
        assert!(empty_store.retain_with_key(&over).is_none());
        assert_eq!(empty_store.bytes, 0);
        assert!(empty_store.map.is_empty());

        let mut lru_store = IccStore::default();
        let mut first_profile = vec![1u8; 64];
        first_profile[0] = 0;
        let (first_ptr, first_len, _) = lru_store
            .retain_with_key(&first_profile)
            .expect("first profile stored");
        for i in 1..=(ICC_MAX_PROFILES + 4) {
            let mut p = vec![2u8; 64];
            p[..std::mem::size_of::<usize>()].copy_from_slice(&i.to_le_bytes());
            assert!(lru_store.retain_with_key(&p).is_some());
        }
        assert!(lru_store.map.len() <= ICC_MAX_PROFILES);
        assert!(!lru_store.retired.is_empty());
        assert_eq!(
            unsafe { std::slice::from_raw_parts(first_ptr, first_len) },
            first_profile.as_slice()
        );

        lru_store.clear();
        assert!(lru_store.map.is_empty());
        assert!(lru_store.retired.is_empty());
        assert_eq!(lru_store.bytes, 0);
        assert_eq!(lru_store.retired_bytes, 0);
    }

    #[test]
    fn icc_store_concurrent_eviction_keeps_pointer_readable() {
        let store = Arc::new(Mutex::new(IccStore::default()));
        let profile_a = vec![0xA5u8; 1024];
        let (ptr, len, _) = store
            .lock()
            .unwrap()
            .retain_with_key(&profile_a)
            .expect("profile A stored");
        let ptr_addr = ptr as usize;

        let mut writers = Vec::new();
        for tid in 0..4usize {
            let store = Arc::clone(&store);
            writers.push(thread::spawn(move || {
                for i in 0..30usize {
                    let mut buf = vec![(tid as u8).wrapping_add(1); 1024];
                    buf[..std::mem::size_of::<usize>()]
                        .copy_from_slice(&(tid * 100 + i).to_le_bytes());
                    let _ = store.lock().unwrap().retain_with_key(&buf);
                    let seen = unsafe { std::slice::from_raw_parts(ptr_addr as *const u8, len) };
                    assert!(seen.iter().all(|&b| b == 0xA5));
                }
            }));
        }
        for w in writers {
            w.join().expect("writer thread panicked");
        }
        let guard = store.lock().unwrap();
        assert!(guard.map.len() <= ICC_MAX_PROFILES);
        assert_eq!(
            unsafe { std::slice::from_raw_parts(ptr_addr as *const u8, len) },
            profile_a.as_slice()
        );
    }

    #[test]
    fn map_jxl_err_classifies_oom_io_cancel_and_decode() {
        let manifest = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
        let mut input =
            FileInput::open(manifest, 0, std::ptr::null_mut()).expect("open Cargo.toml");

        let oom_io = JxlError::IOError(std::io::Error::new(
            std::io::ErrorKind::OutOfMemory,
            "read buffer",
        ));
        assert_eq!(
            map_jxl_err(oom_io, &input, std::ptr::null_mut()),
            OUT_OF_MEMORY
        );

        let not_found =
            JxlError::IOError(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"));
        assert_eq!(
            map_jxl_err(not_found, &input, std::ptr::null_mut()),
            IO_ERROR
        );

        assert_eq!(
            map_jxl_err(
                JxlError::ImageOutOfMemory(1024, 1024),
                &input,
                std::ptr::null_mut()
            ),
            OUT_OF_MEMORY
        );
        assert_eq!(
            map_jxl_err(JxlError::InvalidSignature, &input, std::ptr::null_mut()),
            DECODE_FAILED
        );

        input.reader.was_canceled = true;
        assert_eq!(
            map_jxl_err(JxlError::InvalidSignature, &input, std::ptr::null_mut()),
            CANCELED
        );
    }

    #[test]
    fn preview_terminal_error_classification() {
        assert!(is_terminal_preview_err(CANCELED));
        assert!(is_terminal_preview_err(OUT_OF_MEMORY));
        assert!(is_terminal_preview_err(IO_ERROR));
        assert!(is_terminal_preview_err(UNSUPPORTED));
        assert!(!is_terminal_preview_err(DECODE_FAILED));
    }

    #[test]
    fn memory_admission_respects_available_and_compressed_margin() {
        let frame_bytes = 128 * 1024 * 1024;
        assert!(!frame_fits_available_memory(
            frame_bytes,
            0,
            200 * 1024 * 1024
        ));
        assert!(frame_fits_available_memory(
            frame_bytes,
            0,
            512 * 1024 * 1024
        ));
        assert!(!frame_fits_available_memory(
            frame_bytes,
            300 * 1024 * 1024,
            512 * 1024 * 1024
        ));
    }

    #[test]
    fn downscale_bgra8_box_averaging_and_alpha() {
        let mut runner = DecodeRunner::new(std::ptr::null_mut());
        let mut src = vec![0u8; 16 * 16 * 4];
        for y in 0..16 {
            for x in 0..16 {
                let off = (y * 16 + x) * 4;
                let v = if x % 2 == 0 { 0u8 } else { 255u8 };
                src[off] = v;
                src[off + 1] = v;
                src[off + 2] = v;
                src[off + 3] = 255;
            }
        }
        let mut dst_1x1 = [0u8; 4];
        downscale_bgra8(
            ImagePlane {
                data: &src,
                width: 16,
                height: 16,
            },
            ImagePlaneMut {
                data: &mut dst_1x1,
                width: 1,
                height: 1,
            },
            false,
            &mut runner,
            std::ptr::null_mut(),
        )
        .expect("16x16 -> 1x1 succeeds");
        assert_eq!(dst_1x1, [128, 128, 128, 255]);

        let mut dst_4x4 = vec![0u8; 4 * 4 * 4];
        downscale_bgra8(
            ImagePlane {
                data: &src,
                width: 16,
                height: 16,
            },
            ImagePlaneMut {
                data: &mut dst_4x4,
                width: 4,
                height: 4,
            },
            false,
            &mut runner,
            std::ptr::null_mut(),
        )
        .expect("16x16 -> 4x4 succeeds");
        for px in dst_4x4.as_chunks::<4>().0 {
            assert_eq!(px, &[128, 128, 128, 255]);
        }

        // Alpha-weighted averaging: opaque red + transparent black -> pure red at half alpha
        let alpha_src = [0u8, 0, 255, 255, 0, 0, 0, 0];
        let mut alpha_dst = [0u8; 4];
        downscale_bgra8(
            ImagePlane {
                data: &alpha_src,
                width: 2,
                height: 1,
            },
            ImagePlaneMut {
                data: &mut alpha_dst,
                width: 1,
                height: 1,
            },
            true,
            &mut runner,
            std::ptr::null_mut(),
        )
        .expect("alpha downscale succeeds");
        assert_eq!(alpha_dst, [0, 0, 255, 128]);

        let mut tiny_dst = [0u8; 3];
        assert_eq!(
            downscale_bgra8(
                ImagePlane {
                    data: &alpha_src,
                    width: 2,
                    height: 1,
                },
                ImagePlaneMut {
                    data: &mut tiny_dst,
                    width: 1,
                    height: 1,
                },
                true,
                &mut runner,
                std::ptr::null_mut(),
            ),
            Err(DECODE_FAILED)
        );
        assert_eq!(
            downscale_bgra8(
                ImagePlane {
                    data: &alpha_src,
                    width: 0,
                    height: 1,
                },
                ImagePlaneMut {
                    data: &mut alpha_dst,
                    width: 1,
                    height: 1,
                },
                false,
                &mut runner,
                std::ptr::null_mut(),
            ),
            Err(DECODE_FAILED)
        );

        assert_eq!(fit_within_box(100, 50, 200, 200), (100, 50));
        assert_eq!(fit_within_box(400, 200, 100, 100), (100, 50));
        assert_eq!(fit_within_box(200, 400, 100, 100), (50, 100));
        assert_eq!(fit_within_box(1000, 1, 100, 100), (100, 1));
    }

    #[test]
    fn aligned_buffer_bounds_checks() {
        let mut buf = AlignedBuffer::alloc_zeroed(16).expect("16-byte alloc");
        assert!(buf.as_output_buffer(0, 4).is_err());
        assert!(buf.as_output_buffer(4, 0).is_err());
        assert!(buf.as_output_buffer(5, 4).is_err());
        assert!(buf.as_output_buffer(4, 4).is_ok());

        let too_wide = AlignedBuffer::alloc_zeroed(16).expect("16-byte alloc");
        assert!(too_wide.into_pixel_buffer(4, 2).is_err());

        let zero_dim = AlignedBuffer::alloc_zeroed(16).expect("16-byte alloc");
        assert!(zero_dim.into_pixel_buffer(0, 1).is_err());

        let ok_buf = AlignedBuffer::alloc_zeroed(16).expect("16-byte alloc");
        let mut raw = ok_buf.into_pixel_buffer(2, 2).expect("valid 2x2");
        assert_eq!(raw.width, 2);
        assert_eq!(raw.height, 2);
        assert_eq!(raw.stride, 8);
        unsafe { free_pixel_buffer(&mut raw) };
        assert!(raw.data.is_null());
    }

    #[test]
    fn decode_runner_parallel_serial_cancel_error_and_panic() {
        // Serial and parallel execution each visit every task index exactly once.
        for max_threads in [Some(1), Some(4)] {
            let counts: Vec<AtomicUsize> = (0..64).map(|_| AtomicUsize::new(0)).collect();
            let mut runner = DecodeRunner::new(std::ptr::null_mut());
            runner.max_threads = 4;
            runner
                .run_ordered(64, max_threads, &|i| {
                    counts[i].fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .expect("tasks complete");
            assert!(counts.iter().all(|c| c.load(Ordering::SeqCst) == 1));
        }

        // Mid-run cancellation stops execution and marks `was_canceled`.
        {
            static CANCEL: AtomicI32 = AtomicI32::new(0);
            unsafe extern "C" fn poll_cancel(tok: *mut c_void) -> i32 {
                unsafe { &*(tok as *const AtomicI32) }.load(Ordering::Acquire)
            }
            CANCEL.store(0, Ordering::Release);
            let mut core = IGHostCoreApi {
                log: None,
                alloc: None,
                free: None,
                is_cancellation_requested: Some(poll_cancel),
                get_config_directory: None,
            };
            let _guard = HostCoreGuard::install(&mut core);
            let token = (&CANCEL as *const AtomicI32) as *mut c_void;
            let mut runner = DecodeRunner::new(token);
            runner.max_threads = 4;
            let res = runner.run_ordered(64, Some(4), &|i| {
                if i == 0 {
                    CANCEL.store(1, Ordering::Release);
                }
                thread::sleep(Duration::from_millis(2));
                Ok(())
            });
            assert!(res.is_err());
            assert!(runner.was_canceled());
        }

        // Worker error propagates and stops remaining work.
        let mut err_runner = DecodeRunner::new(std::ptr::null_mut());
        err_runner.max_threads = 4;
        let err_res = err_runner.run_ordered(32, Some(4), &|i| {
            if i == 2 {
                Err(JxlError::InvalidSignature)
            } else {
                Ok(())
            }
        });
        assert!(matches!(err_res, Err(JxlError::InvalidSignature)));

        // Worker panic waits for all active helpers to exit before unwinding.
        let active_helpers = AtomicUsize::new(0);
        let panic_res = catch_unwind(AssertUnwindSafe(|| {
            let mut panic_runner = DecodeRunner::new(std::ptr::null_mut());
            panic_runner.max_threads = 4;
            panic_runner.run_ordered(16, Some(4), &|i| {
                struct ActiveGuard<'a>(&'a AtomicUsize);
                impl Drop for ActiveGuard<'_> {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                active_helpers.fetch_add(1, Ordering::SeqCst);
                let _guard = ActiveGuard(&active_helpers);
                if i == 1 {
                    panic!("intentional worker panic");
                }
                thread::sleep(Duration::from_millis(5));
                Ok(())
            })
        }));
        assert!(panic_res.is_err());
        assert_eq!(active_helpers.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn host_log_callback_and_shutdown_cleanup() {
        static TEST_LOGS: Mutex<Vec<(i32, String)>> = Mutex::new(Vec::new());
        unsafe extern "C" fn capture_log(level: i32, msg: IGStringRef) {
            if !msg.data.is_null() && msg.length > 0 {
                let slice = unsafe { std::slice::from_raw_parts(msg.data, msg.length as usize) };
                TEST_LOGS
                    .lock()
                    .unwrap()
                    .push((level, String::from_utf16_lossy(slice)));
            }
        }
        let mut core = IGHostCoreApi {
            log: Some(capture_log),
            alloc: None,
            free: None,
            is_cancellation_requested: None,
            get_config_directory: None,
        };
        let _guard = HostCoreGuard::install(&mut core);
        TEST_LOGS.lock().unwrap().clear();

        log_host(LOG_WARN, "diagnostic check");
        let status = boundary(|| panic!("boundary panic test"));
        assert_eq!(status, INTERNAL);
        {
            let logs = TEST_LOGS.lock().unwrap();
            assert!(logs
                .iter()
                .any(|(lvl, msg)| *lvl == LOG_WARN && msg == "diagnostic check"));
            assert!(logs
                .iter()
                .any(|(lvl, msg)| *lvl == LOG_ERROR && msg.contains("boundary panic test")));
        }

        let state = STATE.get_or_init(PluginState::new);
        {
            let mut pool = state.decoders.lock().unwrap();
            pool.push(JxlDecoder::new(JxlDecoderOptions::default()));
        }
        {
            let mut icc = state.icc.lock().unwrap();
            assert!(icc.retain_with_key(&[1, 2, 3, 4]).is_some());
        }
        unsafe { shutdown() };
        assert!(state.decoders.lock().unwrap().is_empty());
        {
            let icc = state.icc.lock().unwrap();
            assert!(icc.map.is_empty());
            assert!(icc.retired.is_empty());
            assert_eq!(icc.bytes, 0);
            assert_eq!(icc.retired_bytes, 0);
        }
        let dec = PooledDecoder::checkout();
        drop(dec);
        assert_eq!(state.decoders.lock().unwrap().len(), 1);
    }

    #[test]
    fn manifest_version_matches_cargo_pkg_version() {
        let manifest = include_str!("../igplugin.json");
        let expected = format!("\"version\": \"{}\"", env!("CARGO_PKG_VERSION"));
        assert!(
            manifest.contains(&expected),
            "igplugin.json missing expected version entry {expected}"
        );
    }
}
