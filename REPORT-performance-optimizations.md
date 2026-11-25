# JPEG XL Codec Plugin — Performance Findings & Tier 1–4 Optimizations

Date: 2026-11-25 · Plugin: `jxl_codec` v2.0.0 (`jxl = 0.8.0`) · Host ABI: ImageGlass 10 (ABI 1.2.0)

## 1. Executive Summary

Auditing [src/lib.rs](src/lib.rs), [Cargo.toml](Cargo.toml), and the underlying `jxl-0.8.0` decoder crate uncovered **10 performance & lifecycle improvements** across multi-threading, streaming I/O, pixel format conversion, redundant header parsing, memory allocation, thumbnail scaling, decoder reuse/cleanup, and FFI probe/path overhead.

**All implemented optimizations across [src/lib.rs](src/lib.rs)** preserve portable baseline `x86-64` compatibility (`jxl_simd` selects SSE4.2 / AVX2 / AVX-512 at runtime) with zero new crate dependencies. On this 8-core / 16-thread AMD Ryzen 7 4800H system:
- **Full-resolution decode:** **4.38×–6.04× faster** than baseline v1.2.0 across multi-group RGB/RGBA images.
- **Metadata loading (`load_metadata`):** **2.31×–13.15× faster** (`0.077 ms–0.113 ms` warm across all tested files).
- **Scaled thumbnails (`decode_static_raster_scaled`):** Implemented per ImageGlass ABI 1.2.0; returns `256×256` / `256×128` thumbnails **4.34×–6.21× faster** than baseline v1.2.0 full-size decode using embedded previews when available or exact full-area fractional box downsampling across worker threads.
- **Extension probe (`can_handle_extension`):** **23.8× faster** (`48.4 ns → 2.0 ns` per call, zero heap allocations).
- **Correctness & lifecycle:** **100% byte-for-byte identical BGRA8 pixels and ICC profiles** across all fixtures, zero-aliasing box downsampling, mid-decode cancellation in `0.64 ms`, and clean `shutdown` pool cleanup before `FreeLibrary` DLL unload.

### Measured 3-Stage Results (Baseline v1.2.0 vs Tier 1+2 vs Final v2.0.0)

| Metric / Fixture | Dimensions (Size) | Baseline v1.2.0 | Tier 1 + 2 | Final v2.0.0 | Speedup (vs Base) |
|---|---|---:|---:|---:|---:|
| `can_handle_extension` | 4-probe avg | 48.4 ns | 50.9 ns | **2.0 ns** | **23.8×** |
| **Metadata** `test.jxl` | 1×1 (61 B) | 0.180 ms | 0.108 ms | **0.078 ms** | **2.31×** |
| **Metadata** `rgba_2k.jxl` | 2048×2048 (10.8 KB) | 0.223 ms | 0.102 ms | **0.089 ms** | **2.50×** |
| **Metadata** `rgb_4k.jxl` | 4096×4096 (2.04 MB) | 0.790 ms | 0.118 ms | **0.113 ms** | **6.97×** |
| **Metadata** `rgba_4k.jxl` | 4096×4096 (27.0 MB) | 0.883 ms | 0.129 ms | **0.080 ms** | **11.10×** |
| **Metadata** `rgb_8k4k.jxl` | 8192×4096 (3.11 MB) | 1.012 ms | 0.107 ms | **0.077 ms** | **13.15×** |
| **Decode** `test.jxl` | 1×1 (61 B) | 0.7 ms | **0.5 ms** | **0.5 ms** | **1.48×** |
| **Decode** `rgba_2k.jxl` | 2048×2048 RGBA | 483.2 ms | 88.3 ms | **80.1 ms** | **6.04×** |
| **Decode** `rgb_4k.jxl` | 4096×4096 RGB | 419.4 ms | 118.6 ms | **95.7 ms** | **4.38×** |
| **Decode** `rgba_4k.jxl` | 4096×4096 RGBA | 1474.9 ms | **245.6 ms** | 262.4 ms | **5.62×** |
| **Decode** `rgb_8k4k.jxl` | 8192×4096 RGB | 805.8 ms | **156.9 ms** | 162.5 ms | **4.96×** |
| **Scaled(256)** `rgba_2k.jxl` | → 256×256 | *483.2 ms (full)* | — | **77.8 ms** | **6.21×** |
| **Scaled(256)** `rgb_4k.jxl` | → 256×256 | *419.4 ms (full)* | — | **96.6 ms** | **4.34×** |
| **Scaled(256)** `rgba_4k.jxl` | → 256×256 | *1474.9 ms (full)* | — | **291.1 ms** | **5.07×** |
| **Scaled(256)** `rgb_8k4k.jxl` | → 256×128 | *805.8 ms (full)* | — | **173.9 ms** | **4.63×** |

*Test methodology:* Portable release builds (`opt-level = 3`, `lto = true`, `codegen-units = 1`, `strip = "symbols"`, standard baseline `x86-64` target with `jxl_simd` runtime dispatch), driven through the real C ABI (`LoadLibraryW` → `ig_plugin_get_api` → `can_handle_extension` / `load_metadata` / `decode_static_raster_scaled` / `decode_static_raster` → `free_pixel_buffer` → `FreeLibrary`). Timings report warm median over 20×40,000 extension probes, 50 `load_metadata` calls, and 5 `decode_static_raster` / `decode_static_raster_scaled` calls per fixture. Mid-decode cancellation on `rgb_8k4k.jxl` returned `IGStatus::Canceled (2)` in **0.64 ms** after 3 host polls.

---

## 2. Performance Findings & Implementations (Tiers 1–4)

### Tier 1 — Critical Impact (Multi-× Decode Speedup)

#### 1. Single-threaded decoding (`parallel_runner = None`) — **Implemented**
- **Root cause:** `decoder.process(&mut input, ..., None)` caused `jxl-rs` to fall back to `SequentialRunner` (`num_threads() = 1`). All LF group decoding, adaptive LF smoothing, modular transforms, VarDCT group decoding, and SIMD render pipeline stages ran on a single CPU core.
- **Fix ([`DecodeRunner`](src/lib.rs)):** Implemented `jxl::api::JxlParallelRunner` with a per-decode lazy worker thread pool (`available_parallelism()` threads total, including the calling thread). Worker threads are spawned lazily once per decode, reused across all `run_ordered` phases of that frame, poll `is_cancellation_requested` before each task, and are cleanly `join()`ed in `Drop` so no background threads outlive an FFI call or block Windows `FreeLibrary` unloading.

#### 2. `FileFeed` 1 MiB slice throttling, full-file retention, and double copies — **Implemented**
- **Root cause:** `FileFeed` passed `&[u8]` slices capped at `DECODE_CHUNK` (1 MiB) to `JxlDecoder::process`, starving `jxl-rs` `FrameInfo::fill_sections` parallelism, retaining the entire file in a growing `Vec<u8>`, and performing two `seek` syscalls in `open`.
- **Fix ([`FileInput`](src/lib.rs)):** Implemented `jxl::api::JxlBitstreamInput` directly over `std::fs::File`:
  - `available_bytes()` reports remaining bytes (`length - pos`) so `jxl-rs` batches sections across worker threads.
  - `read()` fills every `IoSliceMut` in order (avoiding Windows `File::read_vectored`'s single-slice behavior), uses bounded read-ahead (`16 KiB` metadata / `1 MiB` decode) or direct reads into large slices, and supports `rewind_to_start()` for embedded preview fallback.
  - `skip()` drains buffered bytes and uses `SeekFrom::Start` for `O(1)` seeks.
  - `FileInput::open` reads length via `file.metadata()?.len()`.

---

### Tier 2 — High Impact (Memory Bandwidth, Redundant Work & Metadata Latency)

#### 3. Post-decode single-threaded RGBA → BGRA swap pass — **Implemented**
- **Root cause:** Requesting `JxlPixelFormat::rgba8` forced a single-threaded `px.swap(0, 2)` pass over the full output buffer after cache eviction.
- **Fix ([`bgra8_pixel_format`](src/lib.rs)):** Requests `JxlColorType::Bgra` (`U8 { bit_depth: 8 }`) directly so `jxl-rs`'s `SaveStage` swaps channels `0` and `2` before SIMD interleaving in L1 cache across worker threads.

#### 4. Double file open and double header decode in `decode_static_raster` — **Implemented**
- **Root cause:** `decode_static_raster` previously ran `decode_info` and `decode_pixels` as two separate file opens and two `JxlDecoder` instances.
- **Fix ([`decode_static_raster`](src/lib.rs)):** Single-pass decode using one `FileInput` and one pooled `JxlDecoder`.

#### 5. Unnecessary frame/pipeline setup & repeated ICC synthesis in `load_metadata` — **Implemented**
- **Root cause:** Stopping at `Event::FrameHeader` triggered TOC parsing and full `LowMemoryRenderPipeline` construction, a dead 4-chunk loop polled for already-available profiles, `std::fs::metadata(&path)` duplicated `stat`, and `try_as_icc()` regenerated + MD5-hashed synthetic sRGB ICC bytes on every `load_metadata` call.
- **Fix ([`load_metadata`](src/lib.rs), [`IccStore`](src/lib.rs)):** Stops immediately at `Event::BasicInfo` with `16 KiB` initial buffering, sets `bgra8_pixel_format` before querying `output_color_profile()`, uses `input.length` for `file_size_bytes`, switches `IccStore` eviction to `VecDeque::pop_front()`, checks exact byte equality on hash lookup, and caches `JxlColorEncoding -> key` entries in `IccStore::retain_profile`.

---

### Tier 3 — Medium Impact (Allocation, Thumbnails & Decoder Reuse)

#### 6. Single-allocation 64-byte base-aligned output buffers — **Implemented**
- **Root cause:** `Vec::resize(bytes, 0)` + `Box::new(pixels)` performed an extra heap allocation for `IGPixelBuffer::release_context`.
- **Fix ([`AlignedBuffer`](src/lib.rs), [`free_pixel_buffer`](src/lib.rs)):** Allocates a single buffer via `std::alloc::alloc_zeroed(Layout::from_size_align(bytes, 64))` and stores `bytes as *mut c_void` directly in `IGPixelBuffer::release_context`. This eliminates the secondary `Box<Vec<u8>>` wrapper and guarantees zero-initialized memory with 64-byte base alignment (individual rows after row 0 remain tightly packed at `stride = width * 4` per the host ABI).

#### 7. Scaled thumbnail decoding (`decode_static_raster_scaled`) — **Implemented**
- **Root cause:** `decode_static_raster_scaled` was `None` in `IGCodecApi`, forcing ImageGlass to decode full 144–576 MP frames for gallery thumbnails.
- **Fix ([`decode_static_raster_scaled`](src/lib.rs), [`downscale_bgra8`](src/lib.rs)):** Implemented ImageGlass ABI 1.2.0 `DecodeStaticRasterScaled`:
  1. **Unscaled fast path:** Returns full image directly when `width <= max_width && height <= max_height`.
  2. **Embedded preview fast path:** When `basic.preview_size` (oriented via `basic.orientation.map_size`) covers `(dst_w, dst_h)` and is smaller than `(width, height)`, rewinds `FileInput` and decodes only the preview frame (`skip_preview = false`).
  3. **Exact full-area parallel box downscaler:** After complete frame decoding (avoiding partial-render black regions that `flush_pixels` could otherwise produce on Modular images), downscales row-major across `DecodeRunner` worker threads using exact reduced rational box weights `(sw / dw, sh / dh)` with `u64` accumulators and premultiplied-alpha weighting when `has_alpha` is true—preventing periodic-pattern aliasing and transparent-black fringing—then immediately frees the temporary full-size buffer.

#### 8. Bounded `JxlDecoder` reuse pool with `shutdown` cleanup — **Implemented**
- **Root cause:** Every call created a fresh `JxlDecoder::new`, rebuilding `BufferRecycler` (~272 `HashMap` buckets plus `PerThreadStorage` for `VarDctBuffers` and modular LZ77 scratch windows).
- **Fix ([`PooledDecoder`](src/lib.rs), [`plugin_shutdown`](src/lib.rs)):** Pools up to `4` reset `JxlDecoder` instances in `PluginState::decoders` across `load_metadata`, `decode_static_raster`, and `decode_static_raster_scaled`, drops decoders above `64 MP` (`DECODER_POOL_MAX_PIXELS`) immediately, and registers `IGPluginApi::shutdown` so pooled decoders and cached ICC entries are explicitly dropped and shrunk before DLL unload.

---

### Tier 4 — Micro-Optimizations & Optional Local Tuning

#### 9. Zero-allocation `can_handle_extension` & direct Windows UTF-16 `PathBuf` — **Implemented**
- **Fix ([`can_handle_extension`](src/lib.rs), [`string_path`](src/lib.rs)):**
  - `can_handle_extension` now checks `ext.length == 4` and compares the 4 UTF-16 code units inline (`[b'.', b'j', b'x', b'l']` with `| 0x20` ASCII folding), removing `String::from_utf16` heap allocation and `catch_unwind` overhead (**23.8× speedup**, `48.4 ns → 2.0 ns`).
  - On Windows, `string_path` uses `OsString::from_wide` to avoid UTF-16 → UTF-8 `String` → UTF-16 round-trip conversion.

#### 10. Portable `x86-64` distribution + optional local `x86-64-v3` / PGO tuning — **Documented**
- Distributed artifacts keep baseline `x86-64` codegen so older non-AVX2 CPUs never hit illegal instructions; `jxl_simd` still dispatches SSE4.2 / AVX2+FMA / AVX-512 SIMD kernels at runtime.
- For private machine-specific builds (`Haswell 2013+` / `Ryzen`) or Profile-Guided Optimization, pass `RUSTFLAGS="-C target-cpu=x86-64-v3"` (optionally combined with `-C profile-use=merged.profdata` via `llvm-profdata`) locally when invoking `cargo build --release`.

---

## 3. Files Modified & Verification

- [src/lib.rs](src/lib.rs): Implemented Tiers 1–4 (`FileInput` with rewind, `DecodeRunner`, `bgra8_pixel_format`, `IccStore::retain_profile`, `AlignedBuffer`, `PooledDecoder`, `plugin_shutdown`, `decode_static_raster_scaled` with embedded preview + exact fractional full-area `downscale_bgra8`, zero-alloc `can_handle_extension`, Windows `OsString::from_wide`).
- [tests/src/main.rs](tests/src/main.rs) & [tests/fixtures/](tests/fixtures/): Expanded regression suite covering `can_handle_extension` edge cases, ICC deduplication, host cancellation, truncated streams, `2048×2048` Modular thumbnail completeness, `1024×1024` 1 px stripe anti-aliasing (`64×64` & `100×100`), premultiplied alpha scaling (`16×16 → 1×1`), embedded preview decoding (`with_preview.jxl`), `PooledDecoder` reuse, and `plugin_shutdown`.
- [README.md](README.md) & [CHANGELOG.md](CHANGELOG.md): Updated feature, architecture, and v2.0.0 release notes.
- Verification:
  - `cargo build --release --offline --locked` — exited 0, zero warnings.
  - `cargo run --release --offline --manifest-path tests/Cargo.toml` — exited 0, zero warnings, `=== ALL TESTS PASSED ===`.
  - 3-stage ABI equivalence & benchmark suite (`bench.exe`) — byte-for-byte identical BGRA8 pixels and ICC profiles across all 5 fixtures, mid-decode cancellation verified, and clean `FreeLibrary` unload.
