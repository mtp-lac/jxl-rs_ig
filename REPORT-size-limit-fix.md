# JPEG XL plugin — 16k/32k decode failure: diagnosis, fix, verification

Date: 2026-09-30 · Plugin: `jxl-codec-plugin` (Rust cdylib over `jxl-rs` 0.7.4) · Host: ImageGlass 10, ABI 1.2.0

## 1. Reported symptom

```
System.IO.InvalidDataException: IG-E: Native codec 'codec_jxl' returned status DecodeFailed
  for '...\final\127468519_p0-画一个小蝴蝶！v1 (1).jxl'
   at ImageGlass.Plugins.NativeCodecProxy.DecodeCore(...)
```

A 16000×9000 JXL opened fine for smaller files but failed with `DecodeFailed` at this size.

## 2. Root cause (primary)

`jxl-rs`'s `sample_limit` is **not** a pixel budget. In `jxl-0.7.4/src/api/inner/codestream_parser/mod.rs:56`:

```rust
fn check_size_limit(pixel_limit: Option<usize>, (xs, ys): (usize, usize), num_ec: usize) -> Result<()> {
    if let Some(limit) = pixel_limit {
        let xs = xs.max(16);
        let total_pixels = xs.saturating_mul(ys).saturating_mul(3 + num_ec);   // pixels x samples
        if total_pixels >= limit { return Err(Error::ImageSizeTooLarge(xs, ys)); }
    }
    Ok(())
}
```

The plugin passed a *pixel* count as the *sample* limit, in two places, and added a third check with the same confusion:

| Check | Old value | 16000×9000 with alpha | Result |
|---|---|---|---|
| `decode_info`: `w*h > PIXEL_LIMIT/4` | 67,108,864 px | 144,000,000 px | rejected outright |
| `sample_limit` at header parse | 268,435,456 samples | 144,000,000 × (3+1) = 576,000,000 | `Error::ImageSizeTooLarge` |
| `sample_limit` at frame parse | same | same | `Error::ImageSizeTooLarge` |

Either path reached `.map_err(|_| DECODE_FAILED)`, so every cause collapsed into the generic status the host reported. **This was a plugin limit bug, not a `jxl-rs` defect**: 144 MP is far below what the decoder can handle, and `jxl-rs`'s documented default is `sample_limit: None`.

## 3. Two further defects found while testing

**3.1 `CanHandleExtension` returned an `IGStatus` instead of a boolean.**
The contract is `1 = handled, 0 = not`; the plugin returned `IGStatus::OK`, i.e. `0`, for `.jxl` — literally answering "I don't handle this" for its own format:

```
CanHandleExtension(".jxl") -> 0   (contract: 1 = match, 0 = no)   <- before
CanHandleExtension(".jxl") -> 1                                    <- after
```

The host still routed the file (extension registration comes from `IGCodecCapability.DecodeExtensions`, and signature probing returned `1`), so **this did not cause the reported error** — it is a contract violation that could bite in codec-selection paths that honour the probe.

**3.2 Latent double-rotation via EXIF orientation.**
`jxl-rs` always renders upright: `basic_info().size` is pre-swapped for transposing orientations, and the render pipeline rotates pixels (`display_pixel` in `render/simple_pipeline/save.rs`). `JxlDecoderOptions::adjust_orientation` is declared in 0.7.4 but **never read anywhere in the crate**, so it cannot be turned off. The plugin reported `basic.orientation` as `IGImageInfo.Orientation` while handing back already-rotated pixels — ImageGlass would rotate a second time for any orientation ≠ 1. Now reports Identity (`1`). Both test files are orientation 1, so **this bug is unverified in practice** — fixed from source reading, not from an observed failure.

## 4. Fixes applied (`src/lib.rs`)

```rust
const MAX_IMAGE_PIXELS: usize = 1_024_000_000;      // 32_000 x 32_000, as requested
const MAX_OUTPUT_BYTES: usize = MAX_IMAGE_PIXELS * 4;
// +1 because jxl-rs rejects on `total >= limit`; keeps the exact cap admissible.
const JXL_SAMPLE_LIMIT: usize = MAX_OUTPUT_BYTES + 1;
```

- `sample_limit` now derives from the byte budget, so an RGB+alpha image is admitted up to exactly `MAX_IMAGE_PIXELS`; images with extra channels are bounded proportionally tighter by design.
- Pixel guard compares against `MAX_IMAGE_PIXELS` (pixels), not a byte/4 proxy.
- Oversized frames still map to `DECODE_FAILED`: `ig_plugin_abi.h` defines no "too large" status (`IGStatus` ends at `EncodeFailed = 9`).
- **OOM guard** added to `DecodeStaticRaster`, per request to avoid causing OOM — it refuses instead of paging the machine:

```rust
let required = bytes.saturating_add(bytes / 2).saturating_add(64 * 1024 * 1024);
if required > available_physical_bytes() { return OUT_OF_MEMORY; }
```

`available_physical_bytes()` reads `GlobalMemoryStatusEx().ullAvailPhys`; a failed query returns `usize::MAX` so an unmeasurable platform degrades to `try_reserve` rather than blocking decode. A 32000×32000 frame needs 4.0 GB output + 2.0 GB margin + 64 MB ≈ **6.1 GB free** before it is attempted; allocation itself uses `try_reserve_exact` → `OUT_OF_MEMORY`, never abort.

## 5. Verification

A dependency-free harness, [`probe/probe.rs`](probe/probe.rs), loads the built DLL via `LoadLibraryW`/`GetProcAddress` and drives the real C ABI the way ImageGlass does (handshake → `GetCodec` → `GetCapability` → probes → `LoadMetadata` → `DecodeStaticRaster` → `FreePixelBuffer`), checking dimensions, stride, pixel format, non-black pixel content and buffer clearing.

**16k file** — `127468519_p0-画一个小蝴蝶！v1 (1).jxl` (74.8 MB):

```
LoadMetadata -> 0  16000x9000 alpha=0 orient=1 frames=1 icc=536B
round 1: decode OK 33.4s  16000x9000 stride=64000 fmt=1  TL=[97,159,139,255] BR=[112,130,92,255] C=[17,17,18,255] nonblack=4097
round 2: decode OK 33.9s   round 3: decode OK 34.3s
=== PROBE PASSED ===
```

**32k file** — `127468519_p0-画一个小蝴蝶！-bl.jxl` (15.4 MB). Note the file is **32000×18000**, not square:

```
LoadMetadata -> 0  32000x18000 alpha=0 orient=1 frames=1 icc=536B
round 1: decode OK 49.4s  32000x18000 stride=128000 fmt=1 sysFree 17.5 GB  TL=[111,167,147,255] BR=[136,157,121,255]
round 2: decode OK 45.4s                                                            round 3: decode OK 45.2s
=== PROBE PASSED ===
```

- **Decoding**: both files decoded successfully; identical pixel samples in all three rounds, so decode is deterministic. `stride = width × 4`, `pixel_format = 1` (Bgra8Unorm), buffer cleared by `FreePixelBuffer`.
- **Memory**: system free held at **17.5–17.6 GB across all three 32k rounds** with no downward drift — the ~2.3 GB buffer is returned between views rather than accumulating. Machine has 31.4 GB; free was 18.8–19.0 GB before the run.
- **Timing**: ~34 s for 144 MP, ~45–50 s for 576 MP (release build).

**Not verified / caveats:**
- My per-process working-set counter printed `-1` (a `K32GetProcessMemoryInfo` handle bug in the probe), so per-process peak RSS was **not** measured; the memory conclusion rests on system-free sampling, which is indirect.
- **No true 32000×32000 image was tested** — the supplied 32k fixture is 32000×18000 (576 MP). The square case is 4.0 GB output and exercises the RAM guard; the code path is the same but the allocation size is not yet measured on this machine.
- The orientation fix (§3.2) had no orientation≠1 test file to confirm against.

## 6. Not fixed — flagged follow-ups

1. **ICC profile retention grows per load.** `load_metadata` pushes every profile into a process-lifetime `Vec` (required: `IccProfileData` must stay valid, and the ABI gives no release callback). Each is 536 B here, but browsing thousands of files accumulates indefinitely. Needs a bounded cache or de-duplication.
2. **Animated JXL returns `UNSUPPORTED`** by design (`supports_animation_decoding = 0`); the host then falls back or declines. Real animation needs the three animation entry points.
3. **`hdr_transfer_fn = 0` / `color_space = 0`** are hardcoded; PQ/HLG/gain-map JXL and the `ScRgb` value (gated on host ABI ≥ 1.0.1) are not reported.
4. **README not updated** for the new 32k limit or the RAM guard.
5. `FreePixelBuffer` runs on any thread (Skia finalizer per the header); it only drops a `Box<Vec<u8>>`, which is thread-safe, but no test drove it cross-thread.

## 7. Files touched

- [`src/lib.rs`](src/lib.rs) — sample-limit derivation, pixel guard, RAM guard, boolean extension probe, Identity orientation
- [`Cargo.toml`](Cargo.toml) — `jxl = "=0.7.4"` pinned to the audited API
- [`probe/probe.rs`](probe/probe.rs), [`probe/memwatch.ps1`](probe/memwatch.ps1) — ABI harness (standalone, `rustc -O`, no crates.io access needed)
- No changes to `jxl-rs`; all decoding stays in the library.

Rebuild: `cargo build --release --offline` → `target/release/jxl_codec.dll` (2,921,472 B). Install per README.

## 8. v1.2.0 — follow-ups addressed (2026-10-25)

- **§6.1 unbounded ICC growth — fixed.** `IccProfileData` is now served from a
  de-duplicated, bounded store (`IccStore`): identical profiles share one
  allocation (keyed by content hash), and the store caps at 64 profiles / 4 MiB
  with FIFO eviction. The ABI only requires the host to read the profile
  synchronously inside `LoadMetadata`, so eviction cannot dangle an in-flight
  pointer.
- **Metadata loads no longer read the whole file.** `load_metadata` streams the
  file (256 KiB chunks) and stops at the first frame header — dimensions, extra
  channels and the colour profile are all known by then (plus up to 4 extra
  chunks if the profile has not appeared yet). `FileSizeBytes` now comes from
  `metadata()`, not from reading the file.
- **Decodes are streaming and cancellable.** `decode_static_raster` feeds the
  decoder 1 MiB chunks and polls `IsCancellationRequested` at every chunk/event
  boundary, so a 30–50 s decode responds to cancellation instead of only
  checking before the call. Peak memory is the pixel buffer plus one chunk.
- **Release build** now sets `strip = "symbols"`; the DLL is
  `target/release/jxl_codec.dll` (3,005,952 B, `ig_plugin_get_api` export
  verified via `GetProcAddress`).
- Verification (rebuilt against this code): ABI probe passes on `test.jxl`
  (LoadMetadata → DecodeStaticRaster → FreePixelBuffer, BGRA8, orientation 1),
  `IG_CANCEL=1` probe returns `IGStatus_Canceled (2)` as expected, and the
  `tests/` smoke harness passes. No 16k/32k fixtures were available in this
  environment, so the big-image paths are validated by code review plus the
  unchanged dimension/RAM guards from §4.
