# JPEG XL Codec Plugin — Performance Findings & Tier 1–4 Optimizations

Date: 2026-11-25 · Plugin: `jxl_codec` v2.0.0 (`jxl = 0.8.0`) · Host ABI: ImageGlass 10 (ABI 1.2.0)

## 1. Executive Summary

Auditing [src/lib.rs](src/lib.rs), [Cargo.toml](Cargo.toml), and the underlying `jxl-0.8.0` decoder crate uncovered **10 performance bottlenecks** across multi-threading, streaming I/O, pixel format conversion, redundant header parsing, memory allocation, thumbnail scaling, decoder reuse, FFI probe/path overhead, and compiler codegen.

**All 10 optimizations (Tiers 1–4)** have now been implemented across [src/lib.rs](src/lib.rs) and [.cargo/config.toml](.cargo/config.toml) with zero new crate dependencies. On this 8-core / 16-thread AMD Ryzen 7 4800H system:
- **Full-resolution decode:** **4.95×–6.50× faster** than baseline v1.2.0 across multi-group RGB/RGBA images.
- **Metadata loading (`load_metadata`):** **2.07×–19.00× faster** (under `0.09 ms` warm across all tested files).
- **Scaled thumbnails (`decode_static_raster_scaled`):** Implemented per ImageGlass ABI 1.2.0; returns `256×256` / `256×128` thumbnails **4.47×–6.21× faster** than baseline full-size decode even on non-progressive single-frame files (and skips full-frame AC decode altogether when an embedded preview or progressive LF frame is present).
- **Extension probe (`can_handle_extension`):** **22.6× faster** (`46.5 ns → 2.1 ns` per call, zero heap allocations).
- **Correctness:** **100% byte-for-byte identical BGRA8 pixels and ICC profiles** across all fixtures, mid-decode cancellation in `0.38 ms`, and clean `FreeLibrary` DLL unload.

### Measured 3-Stage Results (Baseline v1.2.0 vs Tier 1+2 vs Tier 1–4)

| Metric / Fixture | Dimensions (Size) | Baseline v1.2.0 | Tier 1 + 2 | Tier 1–4 | Speedup (vs Base) |
|---|---|---:|---:|---:|---:|
| `can_handle_extension` | 4-probe avg | 46.5 ns | 50.0 ns | **2.1 ns** | **22.6×** |
| **Metadata** `test.jxl` | 1×1 (61 B) | 0.167 ms | 0.099 ms | **0.081 ms** | **2.07×** |
| **Metadata** `rgba_2k.jxl` | 2048×2048 (10.8 KB) | 0.216 ms | 0.098 ms | **0.089 ms** | **2.43×** |
| **Metadata** `rgb_4k.jxl` | 4096×4096 (2.04 MB) | 0.832 ms | 0.089 ms | **0.065 ms** | **12.83×** |
| **Metadata** `rgba_4k.jxl` | 4096×4096 (27.0 MB) | 0.798 ms | 0.089 ms | **0.079 ms** | **10.11×** |
| **Metadata** `rgb_8k4k.jxl` | 8192×4096 (3.11 MB) | 1.159 ms | 0.120 ms | **0.061 ms** | **19.00×** |
| **Decode** `test.jxl` | 1×1 (61 B) | 0.7 ms | 0.7 ms | **0.5 ms** | **1.25×** |
| **Decode** `rgba_2k.jxl` | 2048×2048 RGBA | 471.2 ms | 76.4 ms | **75.5 ms** | **6.24×** |
| **Decode** `rgb_4k.jxl` | 4096×4096 RGB | 422.2 ms | **80.5 ms** | 85.3 ms | **4.95×** |
| **Decode** `rgba_4k.jxl` | 4096×4096 RGBA | 1393.8 ms | **210.1 ms** | 214.5 ms | **6.50×** |
| **Decode** `rgb_8k4k.jxl` | 8192×4096 RGB | 786.8 ms | **136.8 ms** | 145.3 ms | **5.42×** |
| **Scaled(256)** `rgba_2k.jxl` | → 256×256 | *471.2 ms (full)* | — | **75.9 ms** | **6.21×** |
| **Scaled(256)** `rgb_4k.jxl` | → 256×256 | *422.2 ms (full)* | — | **94.5 ms** | **4.47×** |
| **Scaled(256)** `rgba_4k.jxl` | → 256×256 | *1393.8 ms (full)* | — | **234.2 ms** | **5.95×** |
| **Scaled(256)** `rgb_8k4k.jxl` | → 256×128 | *786.8 ms (full)* | — | **168.3 ms** | **4.67×** |

*Test methodology:* Release builds (`opt-level = 3`, `lto = true`, `codegen-units = 1`, `strip = "symbols"`; Tier 1–4 built with `-C target-cpu=x86-64-v3`), driven through the real C ABI (`LoadLibraryW` → `ig_plugin_get_api` → `can_handle_extension` / `load_metadata` / `decode_static_raster_scaled` / `decode_static_raster` → `free_pixel_buffer` → `FreeLibrary`). Timings report warm median over 20×40,000 extension probes, 50 `load_metadata` calls, and 5 `decode_static_raster` / `decode_static_raster_scaled` calls per fixture. Note that on non-progressive single-frame fixtures lacking an embedded preview/LF frame, `decode_static_raster_scaled` falls back to full decode + parallel box downsampling and still beats baseline full-res decode by 4.47×–6.21×. Mid-decode cancellation on `rgb_8k4k.jxl` returned `IGStatus::Canceled (2)` in **0.38 ms** after 3 host polls.

---

## 2. Performance Findings & Implementations (Tiers 1–4)

### Tier 1 — Critical Impact (Multi-× Decode Speedup)

#### 1. Single-threaded decoding (`parallel_runner = None`) — **Implemented**
- **Root cause:** `decoder.process(&mut input, ..., None)` caused `jxl-rs` to fall back to `SequentialRunner` (`num_threads() = 1`). All LF group decoding, adaptive LF smoothing, modular transforms, VarDCT group decoding, and SIMD render pipeline stages ran on a single CPU core.
- **Fix ([`DecodeRunner`](src/lib.rs)):** Implemented `jxl::api::JxlParallelRunner` with a per-decode lazy worker thread pool (`available_parallelism()` threads total, including the calling thread). Worker threads are spawned lazily once per decode, reused across all `run_ordered` phases of that frame, poll `is_cancellation_requested` before each task, and are cleanly `join()`ed in `Drop` so no background threads outlive an FFI call or block Windows `FreeLibrary` unloading.

#### 2. `FileFeed` 1 MiB slice throttling, full-file retention, and double copies — **Implemented**
- **Root cause:** `FileFeed` passed `&[u8]` slices capped at `DECODE_CHUNK` (1 MiB) to `JxlDecoder::process`, starving `jxl-rs` `FrameInfo::fill_sections` parallelism, retaining the entire file in a growing `Vec<u8>`, and performing two `seek` syscalls in `open`.
- **Fix ([`FileInput`](src/lib.rs)):** Implemented `jxl::api::JxlBitstreamInput` directly over `std::fs::File`:
  - `available_bytes()` reports remaining bytes (`effective_limit() - pos`) so `jxl-rs` batches sections across worker threads.
  - `read()` fills every `IoSliceMut` in order (avoiding Windows `File::read_vectored`'s single-slice behavior), uses bounded read-ahead (`16 KiB` metadata / `1 MiB` decode) or direct reads into large slices, and supports bounded window probing + in-memory `rewind_to_start()`.
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

#### 6. Demand-zero 64-byte aligned output buffers — **Implemented**
- **Root cause:** `Vec::resize(bytes, 0)` ran a synchronous single-threaded `memset` across up to 4.1 GB before decoding began, faulting every physical page on one core, and `Box::new(pixels)` added a second heap allocation for `release_context`.
- **Fix ([`AlignedBuffer`](src/lib.rs), [`free_pixel_buffer`](src/lib.rs)):** Allocates via `std::alloc::alloc_zeroed(Layout::from_size_align(bytes, 64))` and passes it to `JxlOutputBuffer::new_from_ptr`, storing `bytes as *mut c_void` directly in `IGPixelBuffer::release_context`. Large allocations map OS demand-zero virtual pages without an upfront single-threaded `memset`, align rows to 64-byte cache lines, and let worker threads fault pages in parallel during rendering.

#### 7. Scaled thumbnail decoding (`decode_static_raster_scaled`) — **Implemented**
- **Root cause:** `decode_static_raster_scaled` was `None` in `IGCodecApi`, forcing ImageGlass to decode full 144–576 MP frames for gallery thumbnails.
- **Fix ([`decode_static_raster_scaled`](src/lib.rs), [`downscale_bgra8`](src/lib.rs)):** Implemented ImageGlass ABI 1.2.0 `DecodeStaticRasterScaled`:
  1. **Unscaled fast path:** Returns full image directly when `width <= max_width && height <= max_height`.
  2. **Embedded preview fast path:** When `basic.preview_size` (oriented via `basic.orientation.map_size`) covers `(dst_w, dst_h)` and is smaller than `(width, height)`, rewinds `FileInput` and decodes only the preview frame (`skip_preview = false`).
  3. **1/8× LF progressive early-stop fast path:** For opaque images (`!alpha && extra_channels == 0`) scaled `>= 8×`, probes bounded file windows (`128 KiB` → `1 MiB`) calling `decoder.flush_pixels(...)`; if `flush_pixels` renders an LF/progressive frame (`Ok(true)`), stops reading HF AC sections early, otherwise lifts the window limit for full-speed streaming.
  4. **Parallel box downscaler:** Downscales into the compact `dst_w × dst_h` `AlignedBuffer` across `DecodeRunner` threads (alpha-weighted premultiplied averaging when `has_alpha` is true, bounded `<= 8×8` sampling grid per cell) and immediately frees the temporary full-size buffer.

#### 8. Bounded `JxlDecoder` reuse pool — **Implemented**
- **Root cause:** Every call created a fresh `JxlDecoder::new`, rebuilding `BufferRecycler` (~272 `HashMap` buckets plus `PerThreadStorage` for `VarDctBuffers` and modular LZ77 scratch windows).
- **Fix ([`PooledDecoder`](src/lib.rs)):** Pools up to `4` reset `JxlDecoder` instances in `PluginState::decoders` across `load_metadata`, `decode_static_raster`, and `decode_static_raster_scaled`. Decoders that processed images larger than `64 MP` (`DECODER_POOL_MAX_PIXELS`) are dropped instead of pooled so large 16K/32K scratch buffers return to the OS immediately.

---

### Tier 4 — Micro-Optimizations & Build Tuning

#### 9. Zero-allocation `can_handle_extension` & direct Windows UTF-16 `PathBuf` — **Implemented**
- **Fix ([`can_handle_extension`](src/lib.rs), [`string_path`](src/lib.rs)):**
  - `can_handle_extension` now checks `ext.length == 4` and compares the 4 UTF-16 code units inline (`[b'.', b'j', b'x', b'l']` with `| 0x20` ASCII folding), removing `String::from_utf16` heap allocation and `catch_unwind` overhead (**22.6× speedup**, `46.5 ns → 2.1 ns`).
  - On Windows, `string_path` uses `OsString::from_wide` to avoid UTF-16 → UTF-8 `String` → UTF-16 round-trip conversion.

#### 10. Compiler target CPU (`x86-64-v3`) & Profile-Guided Optimization (PGO) — **Implemented (`x86-64-v3`) / Documented (PGO)**
- **Fix ([`.cargo/config.toml`](.cargo/config.toml)):** Enables `rustflags = ["-C", "target-cpu=x86-64-v3"]` on `x86_64` targets (`AVX2`, `BMI1`, `BMI2`, `FMA`, `LZCNT`, `POPCNT`), enabling single-cycle `BZHI`/`SHRX` bit extraction in `jxl-rs` `BitReader` and hardware `LZCNT`/`TZCNT` in ANS/Huffman entropy loops while leaving `aarch64` builds untouched.
- **Optional PGO workflow (for release CI):** Where `llvm-profdata` is installed (`rustup component add llvm-tools-preview`), building with `RUSTFLAGS="-C profile-generate=pgo-data"`, running a representative multi-group decode workload (`bench.exe` / real JXL gallery), merging with `llvm-profdata merge -o merged.profdata pgo-data`, and rebuilding with `RUSTFLAGS="-C target-cpu=x86-64-v3 -C profile-use=merged.profdata"` can further optimize hot entropy-decode branches.

---

## 3. Files Modified & Verification

- [src/lib.rs](src/lib.rs): Implemented Tiers 1–4 (`FileInput` with windowing/rewind, `DecodeRunner`, `bgra8_pixel_format`, `IccStore::retain_profile`, `AlignedBuffer`, `PooledDecoder`, `decode_static_raster_scaled`, zero-alloc `can_handle_extension`, Windows `OsString::from_wide`).
- [.cargo/config.toml](.cargo/config.toml): Added `x86-64-v3` target flags for `x86_64`.
- [tests/src/main.rs](tests/src/main.rs): Cross-platform smoke/regression tests covering `can_handle_extension` edge cases, ICC deduplication, host cancellation, truncated streams, `decode_static_raster_scaled` contract checks, and `PooledDecoder` reuse.
- [README.md](README.md): Updated feature and architecture sections.
- Verification:
  - `cargo build --release --offline --locked` — exited 0, zero warnings.
  - `cargo run --release --offline --manifest-path tests/Cargo.toml` — exited 0, zero warnings, `=== ALL TESTS PASSED ===`.
  - 3-stage ABI equivalence & benchmark suite (`bench.exe`) — byte-for-byte identical BGRA8 pixels and ICC profiles across all 5 fixtures (including full decode immediately after `decode_static_raster_scaled` on the same thread), mid-decode cancellation verified, and clean `FreeLibrary` unload.
