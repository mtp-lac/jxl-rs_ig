#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use jxl::api::{
    check_signature, Event, ExtraChannel, JxlBasicInfo, JxlBitstreamInput, JxlColorEncoding,
    JxlColorProfile, JxlColorType, JxlDataFormat, JxlDecoder, JxlDecoderOptions, JxlOutputBuffer,
    JxlParallelRunner, JxlParallelRunnerFun, JxlPixelFormat, JxlSignature,
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

/// 64-byte base alignment for output pixel buffers (`AlignedBuffer` + `alloc_zeroed`).
const PIXEL_BUFFER_ALIGN: usize = 64;

/// Bounded `JxlDecoder` reuse pool. Retains `BufferRecycler` buckets and thread
/// scratch buffers across consecutive views without holding huge 16k/32k buffers.
const MAX_POOLED_DECODERS: usize = 4;
const DECODER_POOL_MAX_PIXELS: usize = 64 * 1024 * 1024;

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
        if self.is_exhausted() {
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
                    let n = self.reader.read_raw(&mut slice[offset..offset + chunk_len])?;
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
/// one allocation. Entries are evicted FIFO when the store exceeds
/// `ICC_MAX_PROFILES` profiles or `ICC_MAX_BYTES` bytes. Also caches simple
/// `JxlColorEncoding` descriptors to avoid regenerating ICC bytes and MD5 sums.
#[derive(Default)]
struct IccStore {
    order: VecDeque<u64>,
    map: HashMap<u64, Box<[u8]>>,
    simple: Vec<(JxlColorEncoding, u32, u64)>,
    bytes: usize,
}

impl IccStore {
    fn retain_with_key(&mut self, profile: &[u8]) -> Option<(*const u8, usize, u64)> {
        if profile.is_empty() {
            return None;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        profile.hash(&mut hasher);
        let mut key = hasher.finish();
        if let Some(existing) = self.map.get(&key) {
            if existing.as_ref() == profile {
                return Some((existing.as_ptr(), existing.len(), key));
            }
            for (&existing_key, existing_val) in &self.map {
                if existing_val.as_ref() == profile {
                    return Some((existing_val.as_ptr(), existing_val.len(), existing_key));
                }
            }
            while self.map.contains_key(&key) {
                key = key.wrapping_add(1);
            }
        }
        while let Some(old) = (self.map.len() >= ICC_MAX_PROFILES
            || self.bytes + profile.len() > ICC_MAX_BYTES)
            .then(|| self.order.pop_front())
            .flatten()
        {
            if let Some(old_box) = self.map.remove(&old) {
                self.bytes -= old_box.len();
            }
            self.simple.retain(|&(_, _, k)| k != old);
        }
        let owned: Box<[u8]> = profile.into();
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
                let target_bits = intensity_target.to_bits();
                if let Some(&(_, _, key)) = self
                    .simple
                    .iter()
                    .find(|(e, t, _)| e == enc && *t == target_bits)
                {
                    if let Some(existing) = self.map.get(&key) {
                        return Some((existing.as_ptr(), existing.len()));
                    }
                }
                #[cfg(jxl_try_as_icc_target)]
                let icc = profile.try_as_icc(intensity_target)?;
                #[cfg(not(jxl_try_as_icc_target))]
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

        let total_workers = max_threads.unwrap_or(usize::MAX).min(self.max_threads).min(num);
        if total_workers <= 1 {
            for i in 0..num {
                if canceled(cancel) {
                    self.shared.canceled.store(true, Ordering::Release);
                    return Err(JxlError::IOError(std::io::Error::other("canceled")));
                }
                fun(i)?;
            }
            return Ok(());
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
            for i in 0..num {
                if canceled(cancel) {
                    self.shared.canceled.store(true, Ordering::Release);
                    return Err(JxlError::IOError(std::io::Error::other("canceled")));
                }
                fun(i)?;
            }
            return Ok(());
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

/// 64-byte base-aligned, zero-initialized pixel buffer (`alloc_zeroed`).
///
/// Uses a single heap allocation whose byte length is stored directly in
/// `IGPixelBuffer::release_context`, avoiding a secondary `Box<Vec<u8>>`
/// allocation. Because the ABI requires tight packing (`stride = width * 4`),
/// only row 0 is guaranteed 64-byte aligned.
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

    fn as_output_buffer(&mut self, height: usize, bytes_per_row: usize) -> JxlOutputBuffer<'_> {
        unsafe {
            JxlOutputBuffer::new_from_ptr(self.ptr.as_ptr(), height, bytes_per_row, bytes_per_row)
        }
    }

    fn into_pixel_buffer(self, width: usize, height: usize) -> IGPixelBuffer {
        let data = self.ptr.as_ptr();
        let release_context = self.len as *mut c_void;
        std::mem::forget(self);
        IGPixelBuffer {
            data,
            width: width as i32,
            height: height as i32,
            stride: (width * 4) as i32,
            pixel_format: PIXEL_BGRA8,
            release_context,
        }
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
        let mut strings: Vec<Vec<u16>> =
            ["Plugin_JxlCodec", "JPEG XL", "2.1.0", "codec_jxl", ".jxl"]
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
            shutdown: Some(plugin_shutdown),
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

unsafe extern "C" fn plugin_shutdown() {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(state) = STATE.get() {
            if let Ok(mut pool) = state.decoders.lock() {
                pool.clear();
                pool.shrink_to_fit();
            }
            if let Ok(mut icc) = state.icc.lock() {
                *icc = IccStore::default();
            }
        }
        HOST_CORE.store(std::ptr::null_mut(), Ordering::Release);
    }));
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
        || width > i32::MAX as usize
        || height > i32::MAX as usize
    {
        return Err(DECODE_FAILED);
    }
    if basic.animation.is_some() {
        return Err(UNSUPPORTED);
    }
    let alpha = basic
        .extra_channels
        .iter()
        .any(|c| matches!(c.ec_type, ExtraChannel::Alpha));
    Ok((width, height, alpha, basic.extra_channels.len()))
}

fn map_jxl_err(err: JxlError, input: &FileInput, cancel: *mut c_void) -> IGStatus {
    if input.reader.was_canceled || canceled(cancel) {
        return CANCELED;
    }
    if input.reader.io_err || matches!(err, JxlError::IOError(_)) {
        return IO_ERROR;
    }
    if matches!(
        err,
        JxlError::OutOfMemory(_) | JxlError::ImageOutOfMemory(_, _)
    ) {
        return OUT_OF_MEMORY;
    }
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
                    return Err(DECODE_FAILED);
                }
            }
            Ok(Event::Complete) => return Ok(()),
            Ok(_) => {}
            Err(err) => return Err(map_jxl_err(err, input, cancel)),
        }
    }
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
        .ok_or(DECODE_FAILED)?;
    // A 32000x32000 frame alone is ~4 GB, and jxl-rs plus the host's own copy
    // need more on top. Refuse into OutOfMemory rather than paging the host.
    let required = bytes
        .saturating_add(bytes / 2)
        .saturating_add(extra_margin_bytes)
        .saturating_add(64 * 1024 * 1024);
    if required > available_physical_bytes() {
        return Err(OUT_OF_MEMORY);
    }
    AlignedBuffer::alloc_zeroed(bytes).ok_or(OUT_OF_MEMORY)
}

fn decode_full_frame(
    decoder: &mut JxlDecoder,
    input: &mut FileInput,
    pixels: &mut AlignedBuffer,
    width: usize,
    height: usize,
    extra_channels: usize,
    runner: &mut DecodeRunner,
    cancel: *mut c_void,
) -> Result<(), IGStatus> {
    input.set_decode_mode();
    if decoder
        .set_pixel_format(bgra8_pixel_format(extra_channels))
        .is_err()
    {
        return Err(DECODE_FAILED);
    }
    let mut out_buf = [pixels.as_output_buffer(height, width * 4)];
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
                    return Err(DECODE_FAILED);
                }
            }
            Ok(Event::BasicInfo) => return Err(DECODE_FAILED),
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

fn gcd_usize(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a.max(1)
}

#[derive(Clone, Copy)]
struct XSpan {
    sx0: usize,
    sx1: usize,
    w_left: u64,
    w_right: u64,
}

fn downscale_bgra8(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    dst: &mut [u8],
    dst_w: usize,
    dst_h: usize,
    has_alpha: bool,
    runner: &mut DecodeRunner,
    cancel: *mut c_void,
) -> Result<(), IGStatus> {
    let gx = gcd_usize(src_w, dst_w);
    let sw = src_w / gx;
    let dw = dst_w / gx;
    let dw_u64 = dw as u64;
    let sw_u64 = sw as u64;

    let gy = gcd_usize(src_h, dst_h);
    let sh = src_h / gy;
    let dh = dst_h / gy;
    let dh_u64 = dh as u64;
    let sh_u64 = sh as u64;

    let total_weight = sw_u64 * sh_u64;
    let half_weight = total_weight / 2;

    let mut x_spans = Vec::with_capacity(dst_w);
    let sw_128 = sw as u128;
    let dw_128 = dw as u128;
    for dx in 0..dst_w {
        let x_start = (dx as u128) * sw_128;
        let x_end = ((dx + 1) as u128) * sw_128;
        let sx0 = (x_start / dw_128) as usize;
        let sx1 = x_end.div_ceil(dw_128) as usize;
        if sx0 + 1 == sx1 {
            x_spans.push(XSpan {
                sx0,
                sx1,
                w_left: sw_u64,
                w_right: 0,
            });
        } else {
            let w_left = (dw_128 - (x_start % dw_128)) as u64;
            let w_right = (x_end - ((sx1 - 1) as u128) * dw_128) as u64;
            x_spans.push(XSpan {
                sx0,
                sx1,
                w_left,
                w_right,
            });
        }
    }

    let src_stride = src_w * 4;
    let dst_stride = dst_w * 4;
    let dst_base = SendMutPtr(dst.as_mut_ptr());
    let max_threads = dst_h.div_ceil(8).max(1);
    let sh_128 = sh as u128;
    let dh_128 = dh as u128;
    let cancel_token = cancel as usize;

    let res = runner.run_ordered(dst_h, Some(max_threads), &|dy| {
        let y_start = (dy as u128) * sh_128;
        let y_end = ((dy + 1) as u128) * sh_128;
        let sy0 = (y_start / dh_128) as usize;
        let sy1 = y_end.div_ceil(dh_128) as usize;
        let single_row = sy0 + 1 == sy1;
        let wy_first = if single_row {
            sh_u64
        } else {
            (dh_128 - (y_start % dh_128)) as u64
        };
        let wy_last = if single_row {
            0
        } else {
            (y_end - ((sy1 - 1) as u128) * dh_128) as u64
        };

        // SAFETY: each `dy` writes strictly to its own disjoint row `dy * dst_stride`.
        let dst_row =
            unsafe { std::slice::from_raw_parts_mut(dst_base.add(dy * dst_stride), dst_stride) };
        if !has_alpha {
            let mut acc = vec![[0u64; 3]; dst_w];
            let mut polled_pixels = 0usize;
            for sy in sy0..sy1 {
                polled_pixels += src_w;
                if polled_pixels >= CANCEL_POLL_BYTES {
                    polled_pixels = 0;
                    if canceled(cancel_token as *mut c_void) {
                        return Err(JxlError::IOError(std::io::Error::other("canceled")));
                    }
                }
                let wy = if sy == sy0 {
                    wy_first
                } else if sy + 1 == sy1 {
                    wy_last
                } else {
                    dh_u64
                };
                let row = &src[sy * src_stride..(sy + 1) * src_stride];
                for (dst_px, span) in acc.iter_mut().zip(x_spans.iter()) {
                    let p0 = span.sx0 * 4;
                    let (rb, rg, rr) = if span.sx0 + 1 == span.sx1 {
                        (
                            (row[p0] as u64) * span.w_left,
                            (row[p0 + 1] as u64) * span.w_left,
                            (row[p0 + 2] as u64) * span.w_left,
                        )
                    } else {
                        let mut mid_b = 0u64;
                        let mut mid_g = 0u64;
                        let mut mid_r = 0u64;
                        let mid = &row[(span.sx0 + 1) * 4..(span.sx1 - 1) * 4];
                        for px in mid.chunks_exact(4) {
                            mid_b += px[0] as u64;
                            mid_g += px[1] as u64;
                            mid_r += px[2] as u64;
                        }
                        let pr = (span.sx1 - 1) * 4;
                        (
                            (row[p0] as u64) * span.w_left
                                + mid_b * dw_u64
                                + (row[pr] as u64) * span.w_right,
                            (row[p0 + 1] as u64) * span.w_left
                                + mid_g * dw_u64
                                + (row[pr + 1] as u64) * span.w_right,
                            (row[p0 + 2] as u64) * span.w_left
                                + mid_r * dw_u64
                                + (row[pr + 2] as u64) * span.w_right,
                        )
                    };
                    dst_px[0] += rb * wy;
                    dst_px[1] += rg * wy;
                    dst_px[2] += rr * wy;
                }
            }
            for (dst_px, out) in acc.iter().zip(dst_row.chunks_exact_mut(4)) {
                out[0] = ((dst_px[0] + half_weight) / total_weight) as u8;
                out[1] = ((dst_px[1] + half_weight) / total_weight) as u8;
                out[2] = ((dst_px[2] + half_weight) / total_weight) as u8;
                out[3] = 255;
            }
        } else {
            let mut acc = vec![[0u64; 4]; dst_w];
            let mut polled_pixels = 0usize;
            for sy in sy0..sy1 {
                polled_pixels += src_w;
                if polled_pixels >= CANCEL_POLL_BYTES {
                    polled_pixels = 0;
                    if canceled(cancel_token as *mut c_void) {
                        return Err(JxlError::IOError(std::io::Error::other("canceled")));
                    }
                }
                let wy = if sy == sy0 {
                    wy_first
                } else if sy + 1 == sy1 {
                    wy_last
                } else {
                    dh_u64
                };
                let row = &src[sy * src_stride..(sy + 1) * src_stride];
                for (dst_px, span) in acc.iter_mut().zip(x_spans.iter()) {
                    let p0 = span.sx0 * 4;
                    let a0 = row[p0 + 3] as u64;
                    let (rba, rga, rra, ra) = if span.sx0 + 1 == span.sx1 {
                        let wa = span.w_left * a0;
                        (
                            (row[p0] as u64) * wa,
                            (row[p0 + 1] as u64) * wa,
                            (row[p0 + 2] as u64) * wa,
                            wa,
                        )
                    } else {
                        let mut mid_ba = 0u64;
                        let mut mid_ga = 0u64;
                        let mut mid_ra = 0u64;
                        let mut mid_a = 0u64;
                        let mid = &row[(span.sx0 + 1) * 4..(span.sx1 - 1) * 4];
                        for px in mid.chunks_exact(4) {
                            let a = px[3] as u64;
                            mid_ba += (px[0] as u64) * a;
                            mid_ga += (px[1] as u64) * a;
                            mid_ra += (px[2] as u64) * a;
                            mid_a += a;
                        }
                        let wa0 = span.w_left * a0;
                        let pr = (span.sx1 - 1) * 4;
                        let war = span.w_right * (row[pr + 3] as u64);
                        (
                            (row[p0] as u64) * wa0
                                + mid_ba * dw_u64
                                + (row[pr] as u64) * war,
                            (row[p0 + 1] as u64) * wa0
                                + mid_ga * dw_u64
                                + (row[pr + 1] as u64) * war,
                            (row[p0 + 2] as u64) * wa0
                                + mid_ra * dw_u64
                                + (row[pr + 2] as u64) * war,
                            wa0 + mid_a * dw_u64 + war,
                        )
                    };
                    dst_px[0] += rba * wy;
                    dst_px[1] += rga * wy;
                    dst_px[2] += rra * wy;
                    dst_px[3] += ra * wy;
                }
            }
            for (dst_px, out) in acc.iter().zip(dst_row.chunks_exact_mut(4)) {
                let sum_a = dst_px[3];
                let out_a = ((sum_a + half_weight) / total_weight).min(255) as u8;
                if sum_a > 0 {
                    let half_a = sum_a / 2;
                    out[0] = ((dst_px[0] + half_a) / sum_a).min(255) as u8;
                    out[1] = ((dst_px[1] + half_a) / sum_a).min(255) as u8;
                    out[2] = ((dst_px[2] + half_a) / sum_a).min(255) as u8;
                } else {
                    out[0] = 0;
                    out[1] = 0;
                    out[2] = 0;
                }
                out[3] = out_a;
            }
        }
        Ok(())
    });
    match res {
        Ok(()) => Ok(()),
        Err(_) if runner.was_canceled() || canceled(cancel) => Err(CANCELED),
        Err(_) => Err(DECODE_FAILED),
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
        let mut input = match FileInput::open(&path, METADATA_READ_BUF, cancel) {
            Ok(f) => f,
            Err(_) => return IO_ERROR,
        };
        let file_size = i64::try_from(input.length).unwrap_or(-1);

        let mut decoder = PooledDecoder::checkout();
        if let Err(status) = read_until_basic_info(&mut decoder, &mut input, cancel) {
            return status;
        }

        let Some(basic) = decoder.basic_info() else {
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
            return DECODE_FAILED;
        }

        // jxl-rs renders upright by default (`adjust_orientation = true`, and the
        // pipeline rotates the pixels), so `basic.size` is already the oriented size
        // and the host must not rotate again — always report Identity.
        let (icc_ptr, icc_len) = match decoder.output_color_profile() {
            Some(profile) => {
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
        let mut input = match FileInput::open(&path, METADATA_READ_BUF, cancel) {
            Ok(f) => f,
            Err(_) => return IO_ERROR,
        };
        let mut decoder = PooledDecoder::checkout();
        if let Err(status) = read_until_basic_info(&mut decoder, &mut input, cancel) {
            return status;
        }

        let Some(basic) = decoder.basic_info() else {
            return DECODE_FAILED;
        };
        let (width, height, _, extra_channels) = match validate_basic_info(basic) {
            Ok(info) => info,
            Err(status) => return status,
        };
        decoder.set_recycle(width.saturating_mul(height) <= DECODER_POOL_MAX_PIXELS);

        let mut pixels = match allocate_checked_frame(width, height, 0) {
            Ok(b) => b,
            Err(status) => return status,
        };

        let mut runner = DecodeRunner::new(cancel);
        if let Err(status) = decode_full_frame(
            &mut decoder,
            &mut input,
            &mut pixels,
            width,
            height,
            extra_channels,
            &mut runner,
            cancel,
        ) {
            return status;
        }

        unsafe {
            *out = pixels.into_pixel_buffer(width, height);
        }
        OK
    }) as i32
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
        let path = match string_path(path) {
            Ok(p) => p,
            Err(s) => return s,
        };
        let mut input = match FileInput::open(&path, METADATA_READ_BUF, cancel) {
            Ok(f) => f,
            Err(_) => return IO_ERROR,
        };
        let mut decoder = PooledDecoder::checkout();
        if let Err(status) = read_until_basic_info(&mut decoder, &mut input, cancel) {
            return status;
        }

        let Some(basic) = decoder.basic_info() else {
            return DECODE_FAILED;
        };
        let (width, height, alpha, extra_channels) = match validate_basic_info(basic) {
            Ok(info) => info,
            Err(status) => return status,
        };
        let preview_oriented = basic
            .preview_size
            .map(|ps| basic.orientation.map_size(ps))
            .filter(|&(pw, ph)| pw > 0 && ph > 0);
        decoder.set_recycle(width.saturating_mul(height) <= DECODER_POOL_MAX_PIXELS);

        let max_w = max_width as usize;
        let max_h = max_height as usize;
        let (dst_w, dst_h) = fit_within_box(width, height, max_w, max_h);

        // Unscaled fast path: source already fits inside max_width x max_height.
        if dst_w == width && dst_h == height {
            let mut pixels = match allocate_checked_frame(width, height, 0) {
                Ok(b) => b,
                Err(status) => return status,
            };
            let mut runner = DecodeRunner::new(cancel);
            if let Err(status) = decode_full_frame(
                &mut decoder,
                &mut input,
                &mut pixels,
                width,
                height,
                extra_channels,
                &mut runner,
                cancel,
            ) {
                return status;
            }
            unsafe {
                *out = pixels.into_pixel_buffer(width, height);
            }
            return OK;
        }

        // Embedded preview fast path: decode only the preview frame if it covers
        // the requested box and is smaller than the full-resolution image.
        if let Some((pw, ph)) = preview_oriented {
            if pw >= dst_w && ph >= dst_h && pw.saturating_mul(ph) < width.saturating_mul(height) {
                if input.rewind_to_start().is_ok() {
                    let mut preview_opts = JxlDecoderOptions::default();
                    preview_opts.sample_limit = Some(JXL_SAMPLE_LIMIT);
                    preview_opts.skip_preview = false;
                    let mut preview_dec = JxlDecoder::new(preview_opts);
                    if read_until_basic_info(&mut preview_dec, &mut input, cancel).is_ok() {
                        if let Ok(mut prev_buf) = allocate_checked_frame(pw, ph, 0) {
                            let mut runner = DecodeRunner::new(cancel);
                            if decode_full_frame(
                                &mut preview_dec,
                                &mut input,
                                &mut prev_buf,
                                pw,
                                ph,
                                extra_channels,
                                &mut runner,
                                cancel,
                            )
                            .is_ok()
                            {
                                let same_aspect =
                                    pw <= max_w && ph <= max_h && (pw * dst_h).abs_diff(ph * dst_w) <= pw.max(ph);
                                if same_aspect {
                                    unsafe {
                                        *out = prev_buf.into_pixel_buffer(pw, ph);
                                    }
                                    return OK;
                                }
                                let thumb_bytes = dst_w.saturating_mul(dst_h).saturating_mul(4);
                                if let Some(mut thumb) = AlignedBuffer::alloc_zeroed(thumb_bytes) {
                                    if downscale_bgra8(
                                        prev_buf.as_slice(),
                                        pw,
                                        ph,
                                        thumb.as_mut_slice(),
                                        dst_w,
                                        dst_h,
                                        alpha,
                                        &mut runner,
                                        cancel,
                                    )
                                    .is_ok()
                                    {
                                        unsafe {
                                            *out = thumb.into_pixel_buffer(dst_w, dst_h);
                                        }
                                        return OK;
                                    }
                                }
                            }
                        }
                    }
                }
                // Rewind for normal full-image decode if preview fallback occurred.
                if input.rewind_to_start().is_err() {
                    return IO_ERROR;
                }
                decoder = PooledDecoder::checkout();
                decoder.set_recycle(width.saturating_mul(height) <= DECODER_POOL_MAX_PIXELS);
                if let Err(status) = read_until_basic_info(&mut decoder, &mut input, cancel) {
                    return status;
                }
            }
        }

        let thumb_bytes = match dst_w.checked_mul(dst_h).and_then(|n| n.checked_mul(4)) {
            Some(n) if n <= MAX_OUTPUT_BYTES && n <= isize::MAX as usize => n,
            _ => return DECODE_FAILED,
        };
        let mut full_pixels = match allocate_checked_frame(width, height, thumb_bytes) {
            Ok(b) => b,
            Err(status) => return status,
        };
        let Some(mut thumb_pixels) = AlignedBuffer::alloc_zeroed(thumb_bytes) else {
            return OUT_OF_MEMORY;
        };

        let mut runner = DecodeRunner::new(cancel);
        if let Err(status) = decode_full_frame(
            &mut decoder,
            &mut input,
            &mut full_pixels,
            width,
            height,
            extra_channels,
            &mut runner,
            cancel,
        ) {
            return status;
        }

        if let Err(status) = downscale_bgra8(
            full_pixels.as_slice(),
            width,
            height,
            thumb_pixels.as_mut_slice(),
            dst_w,
            dst_h,
            alpha,
            &mut runner,
            cancel,
        ) {
            return status;
        }
        drop(full_pixels);
        if canceled(cancel) {
            return CANCELED;
        }

        unsafe {
            *out = thumb_pixels.into_pixel_buffer(dst_w, dst_h);
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

#[cfg(target_os = "windows")]
fn string_path(s: IGStringRef) -> Result<std::path::PathBuf, IGStatus> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let units = unsafe { std::slice::from_raw_parts(s.data, s.length as usize) };
    Ok(std::path::PathBuf::from(OsString::from_wide(units)))
}

#[cfg(not(target_os = "windows"))]
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
