# Changelog

All notable changes to the JPEG XL Codec Plugin for ImageGlass are listed here.

## 2.0.0 — 2026-11-25

A major speed and memory update focused on making large JPEG XL images feel instant to browse and open.

### Faster image opening
- **Multi-core decoding:** Images now decode across all available CPU cores instead of a single thread, speeding up full-resolution opening by roughly **4.4×–6.0×** on multi-core systems while keeping full portable `x86-64` compatibility (`jxl_simd` selects SSE4.2, AVX2, or AVX-512 automatically at runtime).
- **Direct streaming & native BGRA output:** Removed intermediate file-copy buffering, double header parsing, and the extra RGBA-to-BGRA color swap pass after decoding.
- **Single-allocation output buffers:** Output pixel buffers are allocated once on 64-byte base alignment via `alloc_zeroed`, eliminating secondary heap wrapper allocations when handing buffers to ImageGlass.

### Faster folder browsing & thumbnails
- **Scaled thumbnail decoding:** Implemented ImageGlass's native scaled decode API (`DecodeStaticRasterScaled`). Thumbnails use embedded JXL previews when available or parallel full-area box downsampling (with premultiplied-alpha weighting for transparent images), cutting thumbnail work by **4.3×–6.2×** compared to v1.2.0 full-size decoding without downscaling aliasing.
- **Snappier metadata loading:** Folder scans now stop right after reading basic image headers and skip regenerating identical ICC color profiles, cutting metadata load times by **2.3×–13×**.
- **Decoder reuse & clean shutdown:** A small decoder pool reuses internal scratch buffers between consecutive images and frees them when ImageGlass unloads the plugin (`shutdown`), while `.jxl` file extension checks no longer allocate memory.

### Better responsiveness
- **Faster cancellation:** Cancellation is now checked across worker tasks as well as streaming reads, letting long decodes stop within milliseconds when you switch images.

---

## 1.2.0 — 2026-10-25

- Streamed metadata and decode reads in chunks instead of loading entire files upfront, with cancellation checks at chunk boundaries.
- Added a bounded, de-duplicated ICC profile cache (64 profiles / 4 MiB) so browsing large folders does not leak memory.
- Stripped release symbols for a smaller plugin binary.

## 1.1.0 — 2026-09-30

- Raised supported frame limit to `32000×32000` (fix for `jxl-rs` `sample_limit` sample vs. pixel counting).
- Added available physical memory guard before large allocations to return `OutOfMemory` cleanly.
- Fixed `CanHandleExtension` return convention (`1` / `0`) and prevented double EXIF orientation rotation.
