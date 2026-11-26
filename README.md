# JPEG XL Codec Plugin for ImageGlass

This plugin implements a native codec for ImageGlass 10 that adds support for JPEG XL (JXL) image files using the official [jxl-rs](https://github.com/libjxl/jxl-rs) Rust library.

## Features

- Decodes static JPEG XL images (`.jxl` extension)
- Multi-threaded decoding across all available CPU cores via `JxlParallelRunner`
- Fast scaled thumbnail decoding (`decode_static_raster_scaled`) using embedded
  previews and parallel area/box downsampling
- Extracts image metadata (dimensions, color space, alpha channel)
- Reports frame count for static images
- Supports cancellation during metadata and decode operations (polled across input
  reads and parallel worker tasks, so long decodes can be aborted)
- Streaming `JxlBitstreamInput`: `load_metadata` stops at `Event::BasicInfo`
  without reading the full file, while full decodes stream sections from disk
  into `jxl-rs` section buffers for multi-core section parallelism (with the
  compressed file size factored into the available-RAM check)
- 64-byte aligned (base pointer), OS demand-zero output buffers (`alloc_zeroed`)
  with packed `width * 4` rows, plus a bounded `JxlDecoder` reuse pool for lower
  latency across consecutive image loads
- Bounded, de-duplicated ICC profile cache — identical profiles share one
  allocation, profiles larger than 4 MiB are not retained, and evicted entries
  enter a 10-second retirement grace window so concurrent host copies never read
  freed memory
- Size and memory guards: frames above 32000×32000 pixels (or whose BGRA8 stride
  would overflow signed 32-bit `i32`) are refused, and plugin-owned pixel and
  scratch allocations use fallible allocation backed by an available-physical-memory
  check (`GlobalMemoryStatusEx` on Windows, `/proc/meminfo` `MemAvailable` on
  Linux, Mach `host_statistics64` on macOS) so excessive frames return
  `OUT_OF_MEMORY` (note: internal `jxl-rs` / Rust `std` allocations cannot be
  intercepted by `catch_unwind`)
- **Host integration ceiling:** while the plugin admits frames up to `32000×32000`
  (`1,024 MP`), pinned ImageGlass 10 computes `checked(Stride * Height)` in
  signed 32-bit `int` (max `2,147,483,647` bytes ≈ **536.8 MP** for BGRA8), so
  images larger than ~536.8 MP will throw `OverflowException` on the host side
  until ImageGlass widens that multiplication

## Prerequisites

- Rust toolchain (stable channel, 2021 edition)
- .NET 10 SDK (for ImageGlass SDK compatibility, if building against source)

## Building

Default x86-64 builds target baseline `x86-64` so release artifacts run on all
64-bit CPUs without `SIGILL` crashes, while `jxl_simd` still performs runtime
AVX2 / AVX-512 dispatch on supported hardware.

### On Windows

```powershell
# Build for baseline x86-64
cargo build --release --manifest-path Cargo.toml

# Optional opt-in x86-64-v3 (AVX2, BMI1, BMI2, FMA, LZCNT, POPCNT) build
$env:RUSTFLAGS="-C target-cpu=x86-64-v3"; cargo build --release --manifest-path Cargo.toml
```

### On Linux/macOS

```bash
# Build for baseline x86-64
cargo build --release

# Optional opt-in x86-64-v3 build
RUSTFLAGS="-C target-cpu=x86-64-v3" cargo build --release
```

## Output

The build produces a shared library:
- Windows: `target/release/jxl_codec.dll`
- Linux: `target/release/libjxl_codec.so`
- macOS: `target/release/libjxl_codec.dylib`

Release archives also ship platform manifests (`igplugin.json` for Windows,
`igplugin.linux.json` for Linux, and `igplugin.macos.json` for macOS) alongside
`LICENSE` and `THIRD_PARTY_NOTICES.txt`.

## Installation

1. Copy the shared library for your platform:
   - **Windows**: `JxlCodec.dll` (rename `jxl_codec.dll` if built locally)
   - **Linux**: `libjxl_codec.so`
   - **macOS**: `libjxl_codec.dylib`
2. Create a plugin folder in ImageGlass's config directory:
   - **Windows**: `%LocalAppData%\ImageGlass\_plugins\JxlCodec\`
   - **Linux**: `~/.local/share/ImageGlass/_plugins/JxlCodec/`
   - **macOS**: `/Users/<username>/Library/Application Support/ImageGlass/_plugins/JxlCodec/`
3. Copy the library and the matching manifest (renamed to `igplugin.json` in the
   plugin folder — `igplugin.linux.json` on Linux, `igplugin.macos.json` on macOS)
   into this folder
4. Restart ImageGlass
5. Enable the plugin in **Settings > Plugins**

## igplugin.json

```json
{
  "id": "Plugin_JxlCodec",
  "name": "JPEG XL Codec",
  "description": "Native codec plugin for JPEG XL (JXL) image format support using jxl-rs decoder",
  "version": "3.0.0",
  "author": "JXL Codec Developer",
  "website": "https://github.com/libjxl/jxl-rs",
  "kind": "Codec",
  "executable": "JxlCodec.dll"
}
```

## Architecture

The plugin implements the ImageGlass native codec ABI:

1. **`ig_plugin_get_api`** - Entry point exported by the DLL
2. **`get_capability`** - Reports supported extensions and capabilities
3. **`can_handle_extension`** - Matches `.jxl` extension (zero-allocation UTF-16 probe)
4. **`load_metadata`** - Extracts image info at `Event::BasicInfo` using pooled `JxlDecoder`
5. **`decode_static_raster`** - Multi-threaded native BGRA8 decode into 64-byte aligned buffers
6. **`decode_static_raster_scaled`** - Fast thumbnail/preview decode within a `maxWidth × maxHeight` bound
7. **`free_pixel_buffer`** - Thread-safe release of allocated pixel memory
8. **`shutdown`** - Drains pooled decoders and clears the ICC profile cache on plugin unload

Both `load_metadata` and `decode_static_raster` feed the `jxl-rs` decoder directly
from `FileInput` (`JxlBitstreamInput`): `load_metadata` stops as soon as
`Event::BasicInfo` fires (avoiding frame/pipeline allocation), and
`decode_static_raster` streams sections from disk into `jxl-rs` while running
decoding and SIMD BGRA8 rendering across a `JxlParallelRunner` worker pool.
`decode_static_raster_scaled` uses embedded previews when available and parallel
area/box downsampling into the target box.
ICC profiles returned through `IGImageInfo.icc_profile_data` are kept in a
de-duplicated, bounded store (identical profiles and `JxlColorEncoding` entries
share one allocation; the store caps out at 64 profiles / 4 MiB total with a
10-second retirement grace window after LRU eviction, and refuses to retain
individual profiles larger than 4 MiB).

## Color Space Handling

- Embedded ICC profiles (`<= 4 MiB`) and ICC profiles synthesized from
  `JxlColorEncoding` are returned via `IGImageInfo.icc_profile_data` and
  `IGImageInfo.icc_profile_size`; `color_space` and `hdr_transfer_fn` are
  reported as `0` (default/sRGB).
- Decoded pixel buffers are always unpremultiplied 8-bit BGRA (`AlphaMode::Unpremultiplied`),
  matching ImageGlass's `SKAlphaType.Unpremul` expectation.
- CMYK JPEG XL files (detected via `ExtraChannel::Black` or `JxlColorProfile::is_cmyk()`)
  return `UNSUPPORTED` so ImageGlass can fall back to another codec until real
  CMYK-to-RGB color management is implemented.

## Limitations

- Static images only (no animation support currently)
- BGRA8 pixel format output (ImageGlass native format)
- No CMYK output or encoding support (decoder only)

## References

- [ImageGlass Plugin Development Guide](https://imageglass.org/docs/codec-plugins)
- [jxl-rs Documentation](https://docs.rs/jxl/latest/jxl/)
- [ImageGlass SDK GitHub](https://github.com/ImageGlass/SDK)
- [JPEG XL Specification](https://jpegxl.org/)

## License

BSD-3-Clause (same as jxl-rs). See [LICENSE](LICENSE) and [THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt).
