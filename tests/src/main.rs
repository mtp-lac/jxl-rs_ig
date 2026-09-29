//! Quick test to verify the plugin loads and decodes a JXL file
use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;

const PIXEL_BGRA8: i32 = 1;

#[repr(i32)]
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

fn to_wide(s: &str) -> Vec<u16> { OsStr::new(s).encode_wide().collect() }

fn main() {
    // Load the DLL
    let dll_path = std::path::Path::new("target/release/jxl_codec.dll");
    let lib = unsafe { libloading::Library::new(dll_path) }.expect("Failed to load DLL");
    let get_api: libloading::Symbol<GetApiFn> = unsafe { lib.get(b"ig_plugin_get_api") }.expect("Missing ig_plugin_get_api");

    // Minimal host API (no logging/cancellation)
    let host_core = IGHostCoreApi { log: None, alloc: None, free: None, is_cancellation_requested: None, get_config_directory: None };
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
    println!("Extensions: {:?}", (0..cap.decode_extension_count).map(|i| unsafe { std::slice::from_raw_parts((*cap.decode_extensions.add(i)).data, (*cap.decode_extensions.add(i)).length as usize) }.iter().map(|s| String::from_utf16_lossy(s)).collect::<Vec<_>>());

    // Test extension matching
    let ext = ".jxl";
    let wide_ext = to_wide(ext);
    let ext_ref = IGStringRef { data: wide_ext.as_ptr(), length: wide_ext.len() as i32 };
    let matches = unsafe { (codec.can_handle_extension.unwrap())(ext_ref) };
    println!("Extension .jxl matches: {}", matches);

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

    println!("\n=== ALL TESTS PASSED ===");
}