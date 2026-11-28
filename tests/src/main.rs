//! Quick test to verify the plugin loads and decodes a JXL file
use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

const PIXEL_BGRA8: i32 = 1;

#[repr(i32)]
#[derive(Debug, PartialEq, Eq)]
enum IGStatus {
    OK = 0,
    Unsupported = 1,
    Canceled = 2,
    InvalidArg = 3,
    DecodeFailed = 4,
    OutOfMemory = 5,
    Internal = 6,
    NotImplemented = 7,
    IoError = 8,
    EncodeFailed = 9,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IGStringRef {
    data: *const u16,
    length: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IGPixelBuffer {
    data: *mut u8,
    width: i32,
    height: i32,
    stride: i32,
    pixel_format: i32,
    release_context: *mut c_void,
}

struct SendPixelBuffer(IGPixelBuffer);
unsafe impl Send for SendPixelBuffer {}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IGImageInfo {
    width: i32,
    height: i32,
    pixel_format: i32,
    has_alpha: i32,
    hdr_transfer_fn: i32,
    color_space: i32,
    orientation: i32,
    frame_count: i32,
    file_size_bytes: i64,
    icc_profile_data: *const u8,
    icc_profile_size: i32,
}

#[repr(C)]
struct IGCodecCapability {
    struct_size: i32,
    codec_id: IGStringRef,
    codec_name: IGStringRef,
    metadata_priority: i32,
    decode_priority: i32,
    supports_metadata: i32,
    supports_color_profiles: i32,
    supports_static_raster_decoding: i32,
    supports_animation_decoding: i32,
    decode_extension_count: i32,
    decode_extensions: *const IGStringRef,
    supports_static_raster_encoding: i32,
    supports_multiframe_encoding: i32,
    encode_priority: i32,
    encode_extension_count: i32,
    encode_extensions: *const IGStringRef,
}

#[repr(C)]
struct IGPluginInfo {
    plugin_id: IGStringRef,
    name: IGStringRef,
    version: IGStringRef,
    abi_version: i32,
    codec_count: i32,
}

type GetCodecFn = unsafe extern "C" fn(i32, *mut *mut IGCodecApi) -> i32;

#[repr(C)]
struct IGCodecApi {
    struct_size: i32,
    get_capability: Option<unsafe extern "C" fn(*mut *mut IGCodecCapability) -> i32>,
    can_handle_extension: Option<unsafe extern "C" fn(IGStringRef) -> i32>,
    can_handle_signature: Option<unsafe extern "C" fn(*const u8, i32) -> i32>,
    load_metadata: Option<unsafe extern "C" fn(IGStringRef, *mut IGImageInfo, *mut c_void) -> i32>,
    decode_static_raster:
        Option<unsafe extern "C" fn(IGStringRef, i32, *mut IGPixelBuffer, *mut c_void) -> i32>,
    free_pixel_buffer: Option<unsafe extern "C" fn(*mut IGPixelBuffer)>,
    get_animation_info: Option<unsafe extern "C" fn(IGStringRef, *mut c_void, *mut c_void) -> i32>,
    free_animation_info: Option<unsafe extern "C" fn(*mut c_void)>,
    decode_animation_frame:
        Option<unsafe extern "C" fn(IGStringRef, i32, *mut IGPixelBuffer, *mut c_void) -> i32>,
    encode_static_raster: Option<
        unsafe extern "C" fn(IGStringRef, *const IGPixelBuffer, *const c_void, *mut c_void) -> i32,
    >,
    begin_encode_multiframe: Option<
        unsafe extern "C" fn(
            IGStringRef,
            *const c_void,
            *const c_void,
            *mut *mut c_void,
            *mut c_void,
        ) -> i32,
    >,
    encode_frame: Option<
        unsafe extern "C" fn(*mut c_void, *const IGPixelBuffer, *const c_void, *mut c_void) -> i32,
    >,
    end_encode_multiframe: Option<unsafe extern "C" fn(*mut c_void, i32, *mut c_void) -> i32>,
    decode_static_raster_scaled: Option<
        unsafe extern "C" fn(IGStringRef, i32, i32, i32, *mut IGPixelBuffer, *mut c_void) -> i32,
    >,
}

#[repr(C)]
struct IGPluginApi {
    struct_size: i32,
    abi_version: i32,
    info: IGPluginInfo,
    get_codec: GetCodecFn,
    initialize: Option<unsafe extern "C" fn() -> i32>,
    shutdown: Option<unsafe extern "C" fn()>,
    self_test: Option<unsafe extern "C" fn() -> i32>,
}

#[repr(C)]
struct IGHostCoreApi {
    log: Option<unsafe extern "C" fn(i32, IGStringRef)>,
    alloc: Option<unsafe extern "C" fn(usize) -> *mut c_void>,
    free: Option<unsafe extern "C" fn(*mut c_void)>,
    is_cancellation_requested: Option<unsafe extern "C" fn(*mut c_void) -> i32>,
    get_config_directory: Option<unsafe extern "C" fn(*mut u16, i32) -> i32>,
}

#[repr(C)]
struct IGHostApi {
    struct_size: i32,
    abi_version: i32,
    core: *const IGHostCoreApi,
}

type GetApiFn = unsafe extern "C" fn(i32, *const IGHostApi) -> *const IGPluginApi;

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn sentinel_pixel_buffer() -> IGPixelBuffer {
    IGPixelBuffer {
        data: std::ptr::without_provenance_mut(0x1234),
        width: 99,
        height: 99,
        stride: 396,
        pixel_format: 99,
        release_context: std::ptr::without_provenance_mut(0x5678),
    }
}

fn assert_zero_pixel_buffer(buf: &IGPixelBuffer) {
    assert!(buf.data.is_null());
    assert_eq!(buf.width, 0);
    assert_eq!(buf.height, 0);
    assert_eq!(buf.stride, 0);
    assert_eq!(buf.pixel_format, 0);
    assert!(buf.release_context.is_null());
}

/// 256x256 opaque lossless JXL codestream with 1-pixel alternating black/white vertical stripes.
const STRIPES_256X256_JXL: &[u8] = &[
    0xFF, 0x0A, 0x7F, 0x40, 0x24, 0x08, 0x02, 0x01, 0x00, 0x00, 0x01, 0x4B, 0x28, 0x31, 0xDD, 0x8C,
    0xF5, 0xC1, 0x5B, 0x61, 0xB0, 0x50, 0x78, 0x9E, 0x84, 0x82, 0x92, 0x41, 0x04, 0x6C, 0x00, 0x01,
    0x6A, 0xEC, 0xC6, 0x60, 0x2C, 0xEA, 0x43, 0x8C, 0x31, 0xDE, 0xC5, 0xBB, 0x40, 0x08, 0x00, 0x80,
    0xB4, 0x03, 0x00, 0x80, 0x14, 0xFE, 0x00, 0x03, 0xFD, 0x02, 0x00, 0xFC, 0x03, 0x84, 0xA3, 0x8E,
    0x1B, 0x24, 0xB5, 0x55, 0xCB, 0x81, 0xE8, 0xCA, 0x38, 0x82, 0x06,
];

#[cfg(target_os = "windows")]
const DLL_PATH: &str = "target/release/jxl_codec.dll";
#[cfg(target_os = "linux")]
const DLL_PATH: &str = "target/release/libjxl_codec.so";
#[cfg(target_os = "macos")]
const DLL_PATH: &str = "target/release/libjxl_codec.dylib";

fn main() {
    // Load the DLL (or override path via JXL_CODEC_LIB)
    let dll_path_str = std::env::var("JXL_CODEC_LIB").unwrap_or_else(|_| DLL_PATH.to_string());
    let dll_path = std::path::Path::new(&dll_path_str);
    let lib = unsafe { libloading::Library::new(dll_path) }.expect("Failed to load DLL");
    let get_api: libloading::Symbol<GetApiFn> =
        unsafe { lib.get(b"ig_plugin_get_api") }.expect("Missing ig_plugin_get_api");

    // Minimal host API with deterministic cancellation and log capture
    static CANCEL_FLAG: AtomicI32 = AtomicI32::new(0);
    static LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn is_canceled(token: *mut c_void) -> i32 {
        if token.is_null() {
            return 0;
        }
        unsafe { &*(token as *const AtomicI32) }.load(Ordering::Acquire)
    }
    unsafe extern "C" fn host_log(_level: i32, msg: IGStringRef) {
        LOG_COUNT.fetch_add(1, Ordering::Relaxed);
        if !msg.data.is_null() && msg.length > 0 {
            let slice = unsafe { std::slice::from_raw_parts(msg.data, msg.length as usize) };
            let _ = String::from_utf16_lossy(slice);
        }
    }
    let cancel_token = (&CANCEL_FLAG as *const AtomicI32) as *mut c_void;

    let host_core = IGHostCoreApi {
        log: Some(host_log),
        alloc: None,
        free: None,
        is_cancellation_requested: Some(is_canceled),
        get_config_directory: None,
    };
    let host_api = IGHostApi {
        struct_size: std::mem::size_of::<IGHostApi>() as i32,
        abi_version: 1_002_000,
        core: &host_core,
    };

    // Call plugin entry point and check ABI struct sizes and metadata
    let plugin_api = unsafe { get_api(1_002_000, &host_api) };
    if plugin_api.is_null() {
        panic!("Plugin refused to load");
    }
    let plugin_api = unsafe { &*plugin_api };
    assert_eq!(
        plugin_api.struct_size,
        std::mem::size_of::<IGPluginApi>() as i32
    );
    let plugin_name = String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(
            plugin_api.info.name.data,
            plugin_api.info.name.length as usize,
        )
    });
    let plugin_version = String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(
            plugin_api.info.version.data,
            plugin_api.info.version.length as usize,
        )
    });
    assert_eq!(plugin_version, "4.6.0");
    println!("Plugin loaded: {} v{}", plugin_name, plugin_version);

    // Get codec
    let mut codec_ptr: *mut IGCodecApi = std::ptr::null_mut();
    let status = unsafe { (plugin_api.get_codec)(0, &mut codec_ptr) };
    if status != 0 || codec_ptr.is_null() {
        panic!("Failed to get codec, status={}", status);
    }
    let codec = unsafe { &*codec_ptr };
    assert_eq!(codec.struct_size, std::mem::size_of::<IGCodecApi>() as i32);

    // Check capability
    let mut cap_ptr: *mut IGCodecCapability = std::ptr::null_mut();
    let status = unsafe { (codec.get_capability.unwrap())(&mut cap_ptr) };
    if status != 0 {
        panic!("get_capability failed: {}", status);
    }
    let cap = unsafe { &*cap_ptr };
    assert_eq!(
        cap.struct_size,
        std::mem::size_of::<IGCodecCapability>() as i32
    );
    println!(
        "Codec: {} ({})",
        String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(cap.codec_name.data, cap.codec_name.length as usize)
        }),
        String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(cap.codec_id.data, cap.codec_id.length as usize)
        })
    );
    println!(
        "Extensions: {:?}",
        (0..cap.decode_extension_count)
            .map(|i| {
                let ext = unsafe {
                    std::slice::from_raw_parts(
                        (*cap.decode_extensions.add(i as usize)).data,
                        (*cap.decode_extensions.add(i as usize)).length as usize,
                    )
                };
                String::from_utf16_lossy(ext)
            })
            .collect::<Vec<_>>()
    );

    // Test extension matching (case-insensitive and non-matching cases)
    for (ext, expected) in [
        (".jxl", 1),
        (".JXL", 1),
        (".JxL", 1),
        ("jxl", 0),
        (".png", 0),
        ("", 0),
    ] {
        let wide_ext = to_wide(ext);
        let ext_ref = IGStringRef {
            data: wide_ext.as_ptr(),
            length: wide_ext.len() as i32,
        };
        let matches = unsafe { (codec.can_handle_extension.unwrap())(ext_ref) };
        assert_eq!(
            matches, expected,
            "unexpected extension match result for {ext:?}"
        );
    }
    println!("Extension .jxl matches: 1");

    // Test signature matching (codestream, container, and non-JXL)
    let sig_fn = codec.can_handle_signature.unwrap();
    let codestream_sig = [0xFFu8, 0x0A];
    assert_eq!(
        unsafe { sig_fn(codestream_sig.as_ptr(), codestream_sig.len() as i32) },
        1
    );
    let container_sig = [
        0x00u8, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A,
    ];
    assert_eq!(
        unsafe { sig_fn(container_sig.as_ptr(), container_sig.len() as i32) },
        1
    );
    let png_sig = [0x89u8, 0x50, 0x4E, 0x47];
    assert_eq!(unsafe { sig_fn(png_sig.as_ptr(), png_sig.len() as i32) }, 0);
    println!("Signature checks passed");

    // Load metadata on test.jxl
    let path = std::fs::canonicalize("test.jxl").expect("test.jxl not found");
    let wide_path = to_wide(path.to_str().unwrap());
    let path_ref = IGStringRef {
        data: wide_path.as_ptr(),
        length: wide_path.len() as i32,
    };
    let mut img_info = IGImageInfo::default();
    let status =
        unsafe { (codec.load_metadata.unwrap())(path_ref, &mut img_info, std::ptr::null_mut()) };
    if status != 0 {
        panic!("load_metadata failed: {}", status);
    }
    println!(
        "Image: {}x{}, alpha={}, orientation={}, frames={}",
        img_info.width,
        img_info.height,
        img_info.has_alpha,
        img_info.orientation,
        img_info.frame_count
    );

    // Decode test.jxl
    let mut pixel_buf = IGPixelBuffer::default();
    let status = unsafe {
        (codec.decode_static_raster.unwrap())(path_ref, 0, &mut pixel_buf, std::ptr::null_mut())
    };
    if status != 0 {
        panic!("decode_static_raster failed: {}", status);
    }
    println!(
        "Decoded: {}x{}, stride={}, format={}",
        pixel_buf.width, pixel_buf.height, pixel_buf.stride, pixel_buf.pixel_format
    );
    assert_eq!(pixel_buf.pixel_format, PIXEL_BGRA8);
    assert_eq!(pixel_buf.width, img_info.width);
    assert_eq!(pixel_buf.height, img_info.height);
    assert!(!pixel_buf.data.is_null());

    // Check first few pixels
    let pixels = unsafe {
        std::slice::from_raw_parts(
            pixel_buf.data,
            (pixel_buf.height * pixel_buf.stride) as usize,
        )
    };
    println!("First 16 bytes: {:02X?}", &pixels[..16.min(pixels.len())]);

    // Free buffer and assert fields zeroed
    let free_fn = codec.free_pixel_buffer.unwrap();
    unsafe { free_fn(&mut pixel_buf) };
    assert_zero_pixel_buffer(&pixel_buf);
    println!("Buffer freed successfully!");

    // 1. Repeated load_metadata calls deduplicate ICC profile pointer
    let mut img_info_2 = IGImageInfo::default();
    let status =
        unsafe { (codec.load_metadata.unwrap())(path_ref, &mut img_info_2, std::ptr::null_mut()) };
    assert_eq!(status, IGStatus::OK as i32);
    assert_eq!(img_info_2.icc_profile_size, img_info.icc_profile_size);
    if img_info.icc_profile_size > 0 {
        assert_eq!(img_info_2.icc_profile_data, img_info.icc_profile_data);
    }

    // 2. Cancellation callback honored in both load_metadata and decode_static_raster
    CANCEL_FLAG.store(1, Ordering::Release);
    let mut canceled_info = IGImageInfo::default();
    let status =
        unsafe { (codec.load_metadata.unwrap())(path_ref, &mut canceled_info, cancel_token) };
    assert_eq!(status, IGStatus::Canceled as i32);
    let mut canceled_buf = IGPixelBuffer::default();
    let status = unsafe {
        (codec.decode_static_raster.unwrap())(path_ref, 0, &mut canceled_buf, cancel_token)
    };
    assert_eq!(status, IGStatus::Canceled as i32);
    assert!(canceled_buf.data.is_null());
    CANCEL_FLAG.store(0, Ordering::Release);

    // 3. Truncated codestream returns DecodeFailed without hanging and emits host diagnostic log
    let log_before = LOG_COUNT.load(Ordering::Relaxed);
    let pid = std::process::id();
    let trunc_path = std::env::temp_dir().join(format!("jxl_codec_truncated_test_{pid}.jxl"));
    std::fs::write(&trunc_path, [0xFFu8, 0x0A, 0x00]).expect("write truncated test file");
    let wide_trunc = to_wide(trunc_path.to_str().unwrap());
    let trunc_ref = IGStringRef {
        data: wide_trunc.as_ptr(),
        length: wide_trunc.len() as i32,
    };
    let mut trunc_info = IGImageInfo::default();
    let meta_status =
        unsafe { (codec.load_metadata.unwrap())(trunc_ref, &mut trunc_info, std::ptr::null_mut()) };
    let mut trunc_buf = IGPixelBuffer::default();
    let dec_status = unsafe {
        (codec.decode_static_raster.unwrap())(trunc_ref, 0, &mut trunc_buf, std::ptr::null_mut())
    };
    let _ = std::fs::remove_file(&trunc_path);
    assert_eq!(meta_status, IGStatus::DecodeFailed as i32);
    assert_eq!(dec_status, IGStatus::DecodeFailed as i32);
    assert!(trunc_buf.data.is_null());
    assert!(
        LOG_COUNT.load(Ordering::Relaxed) > log_before,
        "expected host log callback on decode failure"
    );

    // 4. Scaled decode checks (1x1 unscaled, real 256x256 -> 64x64 / 32x32 box downscale, invalid args, cancellation)
    let scaled_fn = codec
        .decode_static_raster_scaled
        .expect("decode_static_raster_scaled missing");
    let mut scaled_buf = IGPixelBuffer::default();
    let status = unsafe { scaled_fn(path_ref, 0, 64, 64, &mut scaled_buf, std::ptr::null_mut()) };
    assert_eq!(status, IGStatus::OK as i32);
    assert_eq!(scaled_buf.width, 1);
    assert_eq!(scaled_buf.height, 1);
    assert_eq!(scaled_buf.pixel_format, PIXEL_BGRA8);
    assert!(!scaled_buf.data.is_null());
    let scaled_px = unsafe { std::slice::from_raw_parts(scaled_buf.data, 4) };
    assert_eq!(scaled_px, &[0x00, 0x00, 0x00, 0xFF]);
    unsafe { free_fn(&mut scaled_buf) };
    assert_zero_pixel_buffer(&scaled_buf);

    // Real downscaling fixture: 256x256 alternating black/white columns -> midpoint 127.5 (127..=128 after rounding/dithering, alpha 255)
    let stripes_path = std::env::temp_dir().join(format!("jxl_codec_stripes_test_{pid}.jxl"));
    std::fs::write(&stripes_path, STRIPES_256X256_JXL).expect("write stripes test file");
    let wide_stripes = to_wide(stripes_path.to_str().unwrap());
    let stripes_ref = IGStringRef {
        data: wide_stripes.as_ptr(),
        length: wide_stripes.len() as i32,
    };
    let mut stripes_info = IGImageInfo::default();
    assert_eq!(
        unsafe {
            (codec.load_metadata.unwrap())(stripes_ref, &mut stripes_info, std::ptr::null_mut())
        },
        IGStatus::OK as i32
    );
    assert_eq!(stripes_info.width, 256);
    assert_eq!(stripes_info.height, 256);

    let mut full_stripes = IGPixelBuffer::default();
    assert_eq!(
        unsafe {
            (codec.decode_static_raster.unwrap())(
                stripes_ref,
                0,
                &mut full_stripes,
                std::ptr::null_mut(),
            )
        },
        IGStatus::OK as i32
    );
    assert_eq!(full_stripes.width, 256);
    assert_eq!(full_stripes.height, 256);
    let full_slice = unsafe {
        std::slice::from_raw_parts(
            full_stripes.data,
            (full_stripes.height * full_stripes.stride) as usize,
        )
    };
    assert_eq!(&full_slice[0..4], &[0, 0, 0, 255]);
    assert_eq!(&full_slice[4..8], &[255, 255, 255, 255]);
    unsafe { free_fn(&mut full_stripes) };

    for (box_w, box_h) in [(64, 64), (64, 32)] {
        let mut down_buf = IGPixelBuffer::default();
        assert_eq!(
            unsafe {
                scaled_fn(
                    stripes_ref,
                    0,
                    box_w,
                    box_h,
                    &mut down_buf,
                    std::ptr::null_mut(),
                )
            },
            IGStatus::OK as i32
        );
        assert_eq!(down_buf.width, box_h);
        assert_eq!(down_buf.height, box_h);
        let bytes = (down_buf.height * down_buf.stride) as usize;
        let down_slice = unsafe { std::slice::from_raw_parts(down_buf.data, bytes) };
        for px in down_slice.as_chunks::<4>().0 {
            assert!(
                px[..3].iter().all(|&c| (127..=128).contains(&c)) && px[3] == 255,
                "expected box-averaged pixel near 127.5 with opaque alpha, got {px:?}"
            );
        }
        unsafe { free_fn(&mut down_buf) };
    }

    // 5. Sentinel *out initialization consistency on invalid args and missing files
    let mut bad_full = sentinel_pixel_buffer();
    assert_eq!(
        unsafe {
            (codec.decode_static_raster.unwrap())(path_ref, 1, &mut bad_full, std::ptr::null_mut())
        },
        IGStatus::InvalidArg as i32
    );
    assert_zero_pixel_buffer(&bad_full);

    for (frame_idx, mw, mh) in [(0, 0, 64), (0, 64, -1), (1, 64, 64)] {
        let mut bad_buf = sentinel_pixel_buffer();
        let st = unsafe {
            scaled_fn(
                path_ref,
                frame_idx,
                mw,
                mh,
                &mut bad_buf,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(st, IGStatus::InvalidArg as i32);
        assert_zero_pixel_buffer(&bad_buf);
    }

    let missing_path = std::env::temp_dir().join(format!("jxl_codec_nonexistent_{pid}.jxl"));
    let wide_missing = to_wide(missing_path.to_str().unwrap());
    let missing_ref = IGStringRef {
        data: wide_missing.as_ptr(),
        length: wide_missing.len() as i32,
    };
    let mut missing_full = sentinel_pixel_buffer();
    assert_eq!(
        unsafe {
            (codec.decode_static_raster.unwrap())(
                missing_ref,
                0,
                &mut missing_full,
                std::ptr::null_mut(),
            )
        },
        IGStatus::IoError as i32
    );
    assert_zero_pixel_buffer(&missing_full);

    let mut missing_scaled = sentinel_pixel_buffer();
    assert_eq!(
        unsafe {
            scaled_fn(
                missing_ref,
                0,
                64,
                64,
                &mut missing_scaled,
                std::ptr::null_mut(),
            )
        },
        IGStatus::IoError as i32
    );
    assert_zero_pixel_buffer(&missing_scaled);

    CANCEL_FLAG.store(1, Ordering::Release);
    let mut canceled_scaled = IGPixelBuffer::default();
    let st = unsafe { scaled_fn(path_ref, 0, 64, 64, &mut canceled_scaled, cancel_token) };
    assert_eq!(st, IGStatus::Canceled as i32);
    assert!(canceled_scaled.data.is_null());
    CANCEL_FLAG.store(0, Ordering::Release);

    // 6. Cross-thread free_pixel_buffer
    let mut xthread_buf = IGPixelBuffer::default();
    assert_eq!(
        unsafe {
            (codec.decode_static_raster.unwrap())(
                path_ref,
                0,
                &mut xthread_buf,
                std::ptr::null_mut(),
            )
        },
        IGStatus::OK as i32
    );
    let wrapped = SendPixelBuffer(xthread_buf);
    let freed = std::thread::spawn(move || {
        let mut w = wrapped;
        unsafe { free_fn(&mut w.0) };
        w
    })
    .join()
    .expect("free_pixel_buffer thread panicked");
    assert_zero_pixel_buffer(&freed.0);

    // 7. Concurrent load_metadata reads ICC slices safely across threads
    let meta_fn = codec.load_metadata.unwrap();
    let p1 = path.to_str().unwrap().to_string();
    let p2 = stripes_path.to_str().unwrap().to_string();
    let mut handles = Vec::new();
    for tid in 0..4 {
        let p1 = p1.clone();
        let p2 = p2.clone();
        handles.push(std::thread::spawn(move || {
            let w1 = to_wide(&p1);
            let w2 = to_wide(&p2);
            let r1 = IGStringRef {
                data: w1.as_ptr(),
                length: w1.len() as i32,
            };
            let r2 = IGStringRef {
                data: w2.as_ptr(),
                length: w2.len() as i32,
            };
            for iter in 0..10 {
                let target = if (tid + iter) % 2 == 0 { r1 } else { r2 };
                let mut info = IGImageInfo::default();
                let st = unsafe { meta_fn(target, &mut info, std::ptr::null_mut()) };
                assert_eq!(st, IGStatus::OK as i32);
                if info.icc_profile_size > 0 && !info.icc_profile_data.is_null() {
                    let icc = unsafe {
                        std::slice::from_raw_parts(
                            info.icc_profile_data,
                            info.icc_profile_size as usize,
                        )
                    };
                    let sum: u64 = icc.iter().map(|&b| b as u64).sum();
                    assert!(sum > 0);
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("concurrent load_metadata thread panicked");
    }

    let _ = std::fs::remove_file(&stripes_path);

    // 8. Pooled decoder reuse across back-to-back metadata and full decode
    for _ in 0..2 {
        let mut info_again = IGImageInfo::default();
        assert_eq!(
            unsafe {
                (codec.load_metadata.unwrap())(path_ref, &mut info_again, std::ptr::null_mut())
            },
            IGStatus::OK as i32
        );
        let mut full_again = IGPixelBuffer::default();
        assert_eq!(
            unsafe {
                (codec.decode_static_raster.unwrap())(
                    path_ref,
                    0,
                    &mut full_again,
                    std::ptr::null_mut(),
                )
            },
            IGStatus::OK as i32
        );
        let px_again = unsafe { std::slice::from_raw_parts(full_again.data, 4) };
        assert_eq!(px_again, &[0x00, 0x00, 0x00, 0xFF]);
        unsafe { free_fn(&mut full_again) };
    }

    // 9. Shutdown hook drains caches cleanly and plugin remains reusable afterward
    let shutdown_fn = plugin_api.shutdown.expect("shutdown callback missing");
    unsafe { shutdown_fn() };
    let mut post_info = IGImageInfo::default();
    assert_eq!(
        unsafe { (codec.load_metadata.unwrap())(path_ref, &mut post_info, std::ptr::null_mut()) },
        IGStatus::OK as i32
    );
    let mut post_buf = IGPixelBuffer::default();
    assert_eq!(
        unsafe {
            (codec.decode_static_raster.unwrap())(path_ref, 0, &mut post_buf, std::ptr::null_mut())
        },
        IGStatus::OK as i32
    );
    assert_eq!(
        unsafe { std::slice::from_raw_parts(post_buf.data, 4) },
        &[0x00, 0x00, 0x00, 0xFF]
    );
    unsafe { free_fn(&mut post_buf) };

    // Silence unused variants check while keeping ABI status enum complete
    let _ = (
        IGStatus::Unsupported,
        IGStatus::OutOfMemory,
        IGStatus::Internal,
        IGStatus::NotImplemented,
        IGStatus::IoError,
        IGStatus::EncodeFailed,
    );

    println!("\n=== ALL TESTS PASSED ===");
}
