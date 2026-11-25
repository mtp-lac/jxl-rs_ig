//! Quick test to verify the plugin loads and decodes a JXL file
use std::ffi::c_void;

const PIXEL_BGRA8: i32 = 1;

#[repr(i32)]
#[derive(Debug, PartialEq, Eq)]
enum IGStatus { OK = 0, Unsupported = 1, Canceled = 2, InvalidArg = 3, DecodeFailed = 4, OutOfMemory = 5, Internal = 6, NotImplemented = 7, IoError = 8, EncodeFailed = 9 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IGStringRef { data: *const u16, length: i32 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IGPixelBuffer { data: *mut u8, width: i32, height: i32, stride: i32, pixel_format: i32, release_context: *mut c_void }

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IGImageInfo { width: i32, height: i32, pixel_format: i32, has_alpha: i32, hdr_transfer_fn: i32, color_space: i32, orientation: i32, frame_count: i32, file_size_bytes: i64, icc_profile_data: *const u8, icc_profile_size: i32 }

#[repr(C)]
struct IGCodecCapability {
    struct_size: i32, codec_id: IGStringRef, codec_name: IGStringRef,
    metadata_priority: i32, decode_priority: i32,
    supports_metadata: i32, supports_color_profiles: i32, supports_static_raster_decoding: i32,
    supports_animation_decoding: i32, decode_extension_count: i32,
    decode_extensions: *const IGStringRef, supports_static_raster_encoding: i32,
    supports_multiframe_encoding: i32, encode_priority: i32,
    encode_extension_count: i32, encode_extensions: *const IGStringRef,
}

#[repr(C)]
struct IGPluginInfo { plugin_id: IGStringRef, name: IGStringRef, version: IGStringRef, abi_version: i32, codec_count: i32 }

type GetCodecFn = unsafe extern "C" fn(i32, *mut *mut IGCodecApi) -> i32;

#[repr(C)]
struct IGCodecApi {
    struct_size: i32,
    get_capability: Option<unsafe extern "C" fn(*mut *mut IGCodecCapability) -> i32>,
    can_handle_extension: Option<unsafe extern "C" fn(IGStringRef) -> i32>,
    can_handle_signature: Option<unsafe extern "C" fn(*const u8, i32) -> i32>,
    load_metadata: Option<unsafe extern "C" fn(IGStringRef, *mut IGImageInfo, *mut c_void) -> i32>,
    decode_static_raster: Option<unsafe extern "C" fn(IGStringRef, i32, *mut IGPixelBuffer, *mut c_void) -> i32>,
    free_pixel_buffer: Option<unsafe extern "C" fn(*mut IGPixelBuffer)>,
    get_animation_info: Option<unsafe extern "C" fn(IGStringRef, *mut c_void, *mut c_void) -> i32>,
    free_animation_info: Option<unsafe extern "C" fn(*mut c_void)>,
    decode_animation_frame: Option<unsafe extern "C" fn(IGStringRef, i32, *mut IGPixelBuffer, *mut c_void) -> i32>,
    encode_static_raster: Option<unsafe extern "C" fn() -> i32>,
    begin_encode_multiframe: Option<unsafe extern "C" fn() -> i32>,
    encode_frame: Option<unsafe extern "C" fn() -> i32>,
    end_encode_multiframe: Option<unsafe extern "C" fn() -> i32>,
    decode_static_raster_scaled: Option<unsafe extern "C" fn(IGStringRef, i32, i32, i32, *mut IGPixelBuffer, *mut c_void) -> i32>,
}

#[repr(C)]
struct IGPluginApi {
    struct_size: i32, abi_version: i32, info: IGPluginInfo,
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
struct IGHostApi { struct_size: i32, abi_version: i32, core: *const IGHostCoreApi }

type GetApiFn = unsafe extern "C" fn(i32, *const IGHostApi) -> *const IGPluginApi;

fn to_wide(s: &str) -> Vec<u16> { s.encode_utf16().collect() }

#[cfg(target_os = "windows")]
const DLL_PATH: &str = "target/release/jxl_codec.dll";
#[cfg(target_os = "linux")]
const DLL_PATH: &str = "target/release/libjxl_codec.so";
#[cfg(target_os = "macos")]
const DLL_PATH: &str = "target/release/libjxl_codec.dylib";

fn main() {
    // Load the DLL
    let dll_path = std::path::Path::new(DLL_PATH);
    let lib = unsafe { libloading::Library::new(dll_path) }.expect("Failed to load DLL");
    let get_api: libloading::Symbol<GetApiFn> = unsafe { lib.get(b"ig_plugin_get_api") }.expect("Missing ig_plugin_get_api");

    // Minimal host API with deterministic cancellation support
    static CANCEL_FLAG: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    unsafe extern "C" fn is_canceled(token: *mut c_void) -> i32 {
        if token.is_null() {
            return 0;
        }
        unsafe { &*(token as *const std::sync::atomic::AtomicI32) }
            .load(std::sync::atomic::Ordering::Acquire)
    }
    let cancel_token = (&CANCEL_FLAG as *const std::sync::atomic::AtomicI32) as *mut c_void;

    let host_core = IGHostCoreApi { log: None, alloc: None, free: None, is_cancellation_requested: Some(is_canceled), get_config_directory: None };
    let host_api = IGHostApi { struct_size: std::mem::size_of::<IGHostApi>() as i32, abi_version: 1_002_000, core: &host_core };

    // Call plugin entry point
    let plugin_api = unsafe { get_api(1_002_000, &host_api) };
    if plugin_api.is_null() { panic!("Plugin refused to load"); }
    let plugin_api = unsafe { &*plugin_api };
    println!("Plugin loaded: {} v{}", String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(plugin_api.info.name.data, plugin_api.info.name.length as usize) }), String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(plugin_api.info.version.data, plugin_api.info.version.length as usize) }));

    // Get codec
    let mut codec_ptr: *mut IGCodecApi = std::ptr::null_mut();
    let status = unsafe { (plugin_api.get_codec)(0, &mut codec_ptr) };
    if status != 0 || codec_ptr.is_null() { panic!("Failed to get codec, status={}", status); }
    let codec = unsafe { &*codec_ptr };

    // Check capability
    let mut cap_ptr: *mut IGCodecCapability = std::ptr::null_mut();
    let status = unsafe { (codec.get_capability.unwrap())(&mut cap_ptr) };
    if status != 0 { panic!("get_capability failed: {}", status); }
    let cap = unsafe { &*cap_ptr };
    println!("Codec: {} ({})", String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(cap.codec_name.data, cap.codec_name.length as usize) }), String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(cap.codec_id.data, cap.codec_id.length as usize) }));
    println!("Extensions: {:?}", (0..cap.decode_extension_count).map(|i| {
        let ext = unsafe { std::slice::from_raw_parts((*cap.decode_extensions.add(i as usize)).data, (*cap.decode_extensions.add(i as usize)).length as usize) };
        String::from_utf16_lossy(ext)
    }).collect::<Vec<_>>());

    // Test extension matching (case-insensitive and non-matching cases)
    for (ext, expected) in [(".jxl", 1), (".JXL", 1), (".JxL", 1), ("jxl", 0), (".png", 0), ("", 0)] {
        let wide_ext = to_wide(ext);
        let ext_ref = IGStringRef { data: wide_ext.as_ptr(), length: wide_ext.len() as i32 };
        let matches = unsafe { (codec.can_handle_extension.unwrap())(ext_ref) };
        assert_eq!(matches, expected, "unexpected extension match result for {ext:?}");
    }
    println!("Extension .jxl matches: 1");

    // Test signature matching
    let sig = [0xFF, 0x0A];
    let sig_matches = unsafe { (codec.can_handle_signature.unwrap())(sig.as_ptr(), sig.len() as i32) };
    println!("Signature FF 0A matches: {}", sig_matches);

    // Load metadata
    let path = std::fs::canonicalize("test.jxl").expect("test.jxl not found");
    let wide_path = to_wide(path.to_str().unwrap());
    let path_ref = IGStringRef { data: wide_path.as_ptr(), length: wide_path.len() as i32 };
    let mut img_info = IGImageInfo::default();
    let status = unsafe { (codec.load_metadata.unwrap())(path_ref, &mut img_info, std::ptr::null_mut()) };
    if status != 0 { panic!("load_metadata failed: {}", status); }
    println!("Image: {}x{}, alpha={}, orientation={}, frames={}", img_info.width, img_info.height, img_info.has_alpha, img_info.orientation, img_info.frame_count);

    // Decode
    let mut pixel_buf = IGPixelBuffer::default();
    let status = unsafe { (codec.decode_static_raster.unwrap())(path_ref, 0, &mut pixel_buf, std::ptr::null_mut()) };
    if status != 0 { panic!("decode_static_raster failed: {}", status); }
    println!("Decoded: {}x{}, stride={}, format={}", pixel_buf.width, pixel_buf.height, pixel_buf.stride, pixel_buf.pixel_format);
    assert_eq!(pixel_buf.pixel_format, PIXEL_BGRA8);
    assert_eq!(pixel_buf.width, img_info.width);
    assert_eq!(pixel_buf.height, img_info.height);
    assert!(!pixel_buf.data.is_null());

    // Check first few pixels
    let pixels = unsafe { std::slice::from_raw_parts(pixel_buf.data, (pixel_buf.height * pixel_buf.stride) as usize) };
    println!("First 16 bytes: {:02X?}", &pixels[..16.min(pixels.len())]);

    // Free buffer
    unsafe { (codec.free_pixel_buffer.unwrap())(&mut pixel_buf) };
    println!("Buffer freed successfully!");

    // 1. Repeated load_metadata calls deduplicate ICC profile pointer
    let mut img_info_2 = IGImageInfo::default();
    let status = unsafe { (codec.load_metadata.unwrap())(path_ref, &mut img_info_2, std::ptr::null_mut()) };
    assert_eq!(status, IGStatus::OK as i32);
    assert_eq!(img_info_2.icc_profile_size, img_info.icc_profile_size);
    if img_info.icc_profile_size > 0 {
        assert_eq!(img_info_2.icc_profile_data, img_info.icc_profile_data);
    }

    // 2. Cancellation callback honored in both load_metadata and decode_static_raster
    CANCEL_FLAG.store(1, std::sync::atomic::Ordering::Release);
    let mut canceled_info = IGImageInfo::default();
    let status = unsafe { (codec.load_metadata.unwrap())(path_ref, &mut canceled_info, cancel_token) };
    assert_eq!(status, IGStatus::Canceled as i32);
    let mut canceled_buf = IGPixelBuffer::default();
    let status = unsafe { (codec.decode_static_raster.unwrap())(path_ref, 0, &mut canceled_buf, cancel_token) };
    assert_eq!(status, IGStatus::Canceled as i32);
    assert!(canceled_buf.data.is_null());
    CANCEL_FLAG.store(0, std::sync::atomic::Ordering::Release);

    // 3. Truncated codestream returns DecodeFailed without hanging
    let trunc_path = std::env::temp_dir().join("jxl_codec_truncated_test.jxl");
    std::fs::write(&trunc_path, [0xFFu8, 0x0A, 0x00]).expect("write truncated test file");
    let wide_trunc = to_wide(trunc_path.to_str().unwrap());
    let trunc_ref = IGStringRef { data: wide_trunc.as_ptr(), length: wide_trunc.len() as i32 };
    let mut trunc_info = IGImageInfo::default();
    let meta_status = unsafe { (codec.load_metadata.unwrap())(trunc_ref, &mut trunc_info, std::ptr::null_mut()) };
    let mut trunc_buf = IGPixelBuffer::default();
    let dec_status = unsafe { (codec.decode_static_raster.unwrap())(trunc_ref, 0, &mut trunc_buf, std::ptr::null_mut()) };
    let _ = std::fs::remove_file(&trunc_path);
    assert_eq!(meta_status, IGStatus::DecodeFailed as i32);
    assert_eq!(dec_status, IGStatus::DecodeFailed as i32);
    assert!(trunc_buf.data.is_null());

    // 4. Scaled decode checks (valid fit, invalid args, cancellation, and decoder pool reuse)
    let scaled_fn = codec.decode_static_raster_scaled.expect("decode_static_raster_scaled missing");
    let mut scaled_buf = IGPixelBuffer::default();
    let status = unsafe { scaled_fn(path_ref, 0, 64, 64, &mut scaled_buf, std::ptr::null_mut()) };
    assert_eq!(status, IGStatus::OK as i32);
    assert_eq!(scaled_buf.width, 1);
    assert_eq!(scaled_buf.height, 1);
    assert_eq!(scaled_buf.pixel_format, PIXEL_BGRA8);
    assert!(!scaled_buf.data.is_null());
    let scaled_px = unsafe { std::slice::from_raw_parts(scaled_buf.data, 4) };
    assert_eq!(scaled_px, &[0x00, 0x00, 0x00, 0xFF]);
    unsafe { (codec.free_pixel_buffer.unwrap())(&mut scaled_buf) };

    for (frame_idx, mw, mh) in [(0, 0, 64), (0, 64, -1), (1, 64, 64)] {
        let mut bad_buf = IGPixelBuffer::default();
        let st = unsafe { scaled_fn(path_ref, frame_idx, mw, mh, &mut bad_buf, std::ptr::null_mut()) };
        assert_eq!(st, IGStatus::InvalidArg as i32);
        assert!(bad_buf.data.is_null());
    }

    CANCEL_FLAG.store(1, std::sync::atomic::Ordering::Release);
    let mut canceled_scaled = IGPixelBuffer::default();
    let st = unsafe { scaled_fn(path_ref, 0, 64, 64, &mut canceled_scaled, cancel_token) };
    assert_eq!(st, IGStatus::Canceled as i32);
    assert!(canceled_scaled.data.is_null());
    CANCEL_FLAG.store(0, std::sync::atomic::Ordering::Release);

    // Pooled decoder reuse across back-to-back metadata, scaled, and full decode
    for _ in 0..2 {
        let mut info_again = IGImageInfo::default();
        assert_eq!(unsafe { (codec.load_metadata.unwrap())(path_ref, &mut info_again, std::ptr::null_mut()) }, IGStatus::OK as i32);
        let mut full_again = IGPixelBuffer::default();
        assert_eq!(unsafe { (codec.decode_static_raster.unwrap())(path_ref, 0, &mut full_again, std::ptr::null_mut()) }, IGStatus::OK as i32);
        let px_again = unsafe { std::slice::from_raw_parts(full_again.data, 4) };
        assert_eq!(px_again, &[0x00, 0x00, 0x00, 0xFF]);
        unsafe { (codec.free_pixel_buffer.unwrap())(&mut full_again) };
    }

    // Helper for temporary fixture paths backed by embedded test assets
    struct TempFixture(std::path::PathBuf, Vec<u16>);
    impl TempFixture {
        fn new(name: &str, bytes: &[u8]) -> Self {
            let p = std::env::temp_dir().join(format!("jxl_ig_test_{name}"));
            std::fs::write(&p, bytes).expect("write fixture");
            let w = to_wide(p.to_str().unwrap());
            Self(p, w)
        }
        fn as_ref(&self) -> IGStringRef {
            IGStringRef { data: self.1.as_ptr(), length: self.1.len() as i32 }
        }
    }
    impl Drop for TempFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    // 5. [P1 regression] 2048x2048 opaque Modular gradient scaled to 256x256 has zero black pixels
    let mod_fix = TempFixture::new("modular_2048.jxl", include_bytes!("../fixtures/modular_2048.jxl"));
    let mut mod_scaled = IGPixelBuffer::default();
    let st = unsafe { scaled_fn(mod_fix.as_ref(), 0, 256, 256, &mut mod_scaled, std::ptr::null_mut()) };
    assert_eq!(st, IGStatus::OK as i32);
    assert_eq!((mod_scaled.width, mod_scaled.height), (256, 256));
    let mod_bytes = unsafe { std::slice::from_raw_parts(mod_scaled.data, 256 * 256 * 4) };
    let black_px = mod_bytes.chunks_exact(4).filter(|p| p[0] == 0 && p[1] == 0 && p[2] == 0).count();
    assert_eq!(black_px, 0, "unexpected black pixels in 2048x2048 modular thumbnail");
    // Top row (~#4080C0 -> BGRA [0xC0, 0x80, 0x40, 0xFF]) & bottom row (~#C08040 -> BGRA [0x40, 0x80, 0xC0, 0xFF])
    assert!((mod_bytes[0] as i32 - 0xC0).abs() <= 4 && (mod_bytes[2] as i32 - 0x40).abs() <= 4);
    let last_off = (256 * 256 - 1) * 4;
    assert!((mod_bytes[last_off] as i32 - 0x40).abs() <= 4 && (mod_bytes[last_off + 2] as i32 - 0xC0).abs() <= 4);
    unsafe { (codec.free_pixel_buffer.unwrap())(&mut mod_scaled) };
    println!("Modular 2048x2048 -> 256x256 thumbnail: 0 black pixels (PASS)");

    // 6. [P2 regression] 1024x1024 alternating 1px black/white columns downscales to uniform gray (no aliasing)
    let stripe_fix = TempFixture::new("stripes_1024.jxl", include_bytes!("../fixtures/stripes_1024.jxl"));
    for (target_dim, tol) in [(64, 1), (100, 8)] {
        let mut stripe_buf = IGPixelBuffer::default();
        let st = unsafe { scaled_fn(stripe_fix.as_ref(), 0, target_dim, target_dim, &mut stripe_buf, std::ptr::null_mut()) };
        assert_eq!(st, IGStatus::OK as i32);
        assert_eq!((stripe_buf.width, stripe_buf.height), (target_dim, target_dim));
        let len = (stripe_buf.width * stripe_buf.height * 4) as usize;
        let data = unsafe { std::slice::from_raw_parts(stripe_buf.data, len) };
        for px in data.chunks_exact(4) {
            assert!((px[0] as i32 - 128).abs() <= tol && (px[1] as i32 - 128).abs() <= tol && (px[2] as i32 - 128).abs() <= tol && px[3] == 255,
                "aliasing detected at {target_dim}x{target_dim}: got pixel {px:?}");
        }
        unsafe { (codec.free_pixel_buffer.unwrap())(&mut stripe_buf) };
    }
    println!("Stripes 1024x1024 -> 64x64 & 100x100 anti-aliased box average (PASS)");

    // 7. Premultiplied alpha box downscaling prevents dark transparent-black fringing
    let alpha_fix = TempFixture::new("alpha_16.jxl", include_bytes!("../fixtures/alpha_16.jxl"));
    let mut alpha_buf = IGPixelBuffer::default();
    let st = unsafe { scaled_fn(alpha_fix.as_ref(), 0, 1, 1, &mut alpha_buf, std::ptr::null_mut()) };
    assert_eq!(st, IGStatus::OK as i32);
    assert_eq!((alpha_buf.width, alpha_buf.height), (1, 1));
    let apx: [u8; 4] = unsafe { std::slice::from_raw_parts(alpha_buf.data, 4) }
        .try_into()
        .expect("4-byte pixel");
    assert!(apx[0] == 0 && apx[1] == 0 && apx[2] == 255 && (apx[3] as i32 - 128).abs() <= 1,
        "unexpected alpha downscale pixel: {apx:?}");
    unsafe { (codec.free_pixel_buffer.unwrap())(&mut alpha_buf) };
    println!("Alpha premultiplied 16x16 -> 1x1: {apx:?} (PASS)");

    // 8. Embedded preview fast path verification (16x16 preview frame inside larger image)
    let prev_fix = TempFixture::new("with_preview.jxl", include_bytes!("../fixtures/with_preview.jxl"));
    let mut prev_info = IGImageInfo::default();
    assert_eq!(unsafe { (codec.load_metadata.unwrap())(prev_fix.as_ref(), &mut prev_info, std::ptr::null_mut()) }, IGStatus::OK as i32);
    assert!(prev_info.width > 16 && prev_info.height > 16);
    let mut prev_16 = IGPixelBuffer::default();
    assert_eq!(unsafe { scaled_fn(prev_fix.as_ref(), 0, 16, 16, &mut prev_16, std::ptr::null_mut()) }, IGStatus::OK as i32);
    assert!(prev_16.width <= 16 && prev_16.height <= 16 && prev_16.width > 0 && prev_16.height > 0);
    let prev_16_bytes = unsafe { std::slice::from_raw_parts(prev_16.data, (prev_16.height * prev_16.stride) as usize) };
    assert!(prev_16_bytes.iter().any(|&b| b != 0 && b != 255), "preview 16x16 pixels unexpectedly blank");
    unsafe { (codec.free_pixel_buffer.unwrap())(&mut prev_16) };
    let mut prev_8 = IGPixelBuffer::default();
    assert_eq!(unsafe { scaled_fn(prev_fix.as_ref(), 0, 8, 8, &mut prev_8, std::ptr::null_mut()) }, IGStatus::OK as i32);
    assert!(prev_8.width <= 8 && prev_8.height <= 8 && prev_8.width > 0 && prev_8.height > 0);
    let prev_8_bytes = unsafe { std::slice::from_raw_parts(prev_8.data, (prev_8.height * prev_8.stride) as usize) };
    assert!(prev_8_bytes.iter().any(|&b| b != 0 && b != 255), "preview 8x8 pixels unexpectedly blank");
    unsafe { (codec.free_pixel_buffer.unwrap())(&mut prev_8) };
    println!("Embedded preview with_preview.jxl ({}x{}) scaled cleanly (PASS)", prev_info.width, prev_info.height);

    // 8b. Cancellation during 1x1 downscale returns Canceled with no output buffer
    CANCEL_FLAG.store(1, std::sync::atomic::Ordering::Release);
    let mut canceled_1x1 = IGPixelBuffer::default();
    let st = unsafe { scaled_fn(mod_fix.as_ref(), 0, 1, 1, &mut canceled_1x1, cancel_token) };
    assert_eq!(st, IGStatus::Canceled as i32);
    assert!(canceled_1x1.data.is_null());
    CANCEL_FLAG.store(0, std::sync::atomic::Ordering::Release);
    println!("1x1 downscale cancellation (PASS)");

    // 9. [P2 regression] Plugin shutdown callback cleans pooled state and allows clean re-entry/unload
    let shutdown_fn = plugin_api.shutdown.expect("IGPluginApi::shutdown must be registered");
    unsafe { shutdown_fn() };
    let plugin_api_2 = unsafe { get_api(1_002_000, &host_api) };
    assert!(!plugin_api_2.is_null(), "plugin re-init after shutdown failed");
    let mut full_after_shutdown = IGPixelBuffer::default();
    assert_eq!(unsafe { (codec.decode_static_raster.unwrap())(path_ref, 0, &mut full_after_shutdown, std::ptr::null_mut()) }, IGStatus::OK as i32);
    unsafe { (codec.free_pixel_buffer.unwrap())(&mut full_after_shutdown) };
    unsafe { shutdown_fn() };
    drop(lib);
    println!("Shutdown cleanup & DLL unload (PASS)");

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