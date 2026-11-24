# Changelog

All notable changes to the JPEG XL Codec Plugin for ImageGlass are listed here.

## 2.0.0 — 2026-11-25

A major speed and memory update focused on making large JPEG XL images feel instant to browse and open.

### Faster image opening
- **Multi-core decoding:** Images now decode across all available CPU cores instead of a single thread, speeding up full-resolution opening by roughly **5×–6.5×** on multi-core systems.
- **Direct streaming & native BGRA output:** Removed intermediate file-copy buffering, double header parsing, and the extra RGBA-to-BGRA color swap pass after decoding.
- **Smarter memory allocation:** Large image buffers are allocated on 64-byte boundaries without a slow upfront zero-fill pass, so worker threads initialize memory in parallel as they render.
- **Modern x86-64 CPU support:** x86-64 builds now enable `x86-64-v3` instructions (AVX2, BMI2, FMA, LZCNT) for faster entropy decoding.

### Faster folder browsing & thumbnails
- **Scaled thumbnail decoding:** Implemented ImageGlass's native scaled decode API (`DecodeStaticRasterScaled`). Thumbnails now use embedded JXL previews when available, stop early on progressive LF data for large opaque images, or downsample in parallel—making thumbnail generation **4.5×–6.2× faster** than full-size decoding before.
- **Snappier metadata loading:** Folder scans now stop right after reading basic image headers and skip regenerating identical ICC color profiles, cutting metadata load times by **2×–19×**.
- **Decoder reuse & zero-alloc probes:** A small decoder pool reuses internal scratch buffers between consecutive images, and `.jxl` file extension checks no longer allocate memory.

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
