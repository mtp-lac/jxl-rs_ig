# JPEG XL Codec Plugin for ImageGlass

This plugin implements a native codec for ImageGlass 10 that adds support for JPEG XL (JXL) image files using the official [jxl-rs](https://github.com/libjxl/jxl-rs) Rust library.

## Features

- Decodes static JPEG XL images (.jxl extension)
- Multi-threaded decoding across all available CPU cores via `JxlParallelRunner`
- Fast scaled thumbnail decoding (`decode_static_raster_scaled`) using embedded
  previews, 1/8× LF progressive early-stop for opaque images, and parallel box
  downsampling
- Extracts image metadata (dimensions, color space, alpha channel)
- Reports frame count for static images
- Supports cancellation during metadata and decode operations (polled across input
  reads and parallel worker tasks, so long decodes can be aborted)
- Streaming `JxlBitstreamInput`: metadata loads stop at `Event::BasicInfo` and
  decodes stream directly into `jxl-rs` section buffers without retaining the
  file in memory
- 64-byte aligned, OS demand-zero output buffers (`alloc_zeroed`) and a bounded
  `JxlDecoder` reuse pool for lower latency across consecutive image loads
- Bounded, de-duplicated ICC profile cache — browsing thousands of files does
  not accumulate memory
- Size guards: frames above 32000×32000 are refused, and huge allocations are
  backed by an available-physical-memory check before allocation (OOM is
  reported, never a crash)

## Prerequisites

- Rust toolchain (stable channel, 2021 edition)
- .NET 10 SDK (for ImageGlass SDK compatibility, if building against source)

## Building

### On Windows

```powershell
# Build for x64
cargo build --release --manifest-path Cargo.toml
```

### On Linux/macOS

```bash
cargo build --release
```

## Output

The build produces a shared library:
- Windows: `target/release/jxl_codec.dll`
- Linux: `target/release/libjxl_codec.so`
- macOS: `target/release/libjxl_codec.dylib`

## Installation

1. Rename the library to `JxlCodec.dll` (on Windows) or the appropriate platform name
2. Create a plugin folder in ImageGlass's config directory:
   - **Windows**: `%LocalAppData%\ImageGlass\_plugins\JxlCodec\`
   - **Linux**: `~/.local/share/ImageGlass/_plugins/JxlCodec/`
   - **macOS**: `/Users/<username>/Library/Application Support/ImageGlass/_plugins/JxlCodec/`
3. Copy the DLL file and `igplugin.json` manifest into this folder
4. Restart ImageGlass
5. Enable the plugin in **Settings > Plugins**

## igplugin.json

```json
{
  "id": "Plugin_JxlCodec",
  "name": "JPEG XL Codec",
  "description": "Native codec plugin for JPEG XL (JXL) image format support using jxl-rs decoder",
  "version": "2.0.0",
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

Both `load_metadata` and `decode_static_raster` feed the `jxl-rs` decoder directly
from `FileInput` (`JxlBitstreamInput`): `load_metadata` stops as soon as
`Event::BasicInfo` fires (avoiding frame/pipeline allocation), and
`decode_static_raster` streams directly into `jxl-rs` sections while running
decoding and SIMD BGRA8 rendering across a `JxlParallelRunner` worker pool.
`decode_static_raster_scaled` uses embedded previews when available, 1/8× LF
early-stop for opaque images, and parallel box downsampling into the target box.
ICC profiles returned through `IGImageInfo.IccProfileData` are kept in a
de-duplicated, bounded store (identical profiles and `JxlColorEncoding` entries
share one allocation; the store caps out at 64 profiles / 4 MiB with FIFO
eviction) — the ABI only requires the host to read the profile synchronously
inside `LoadMetadata`.

## Color Space Handling

The plugin currently reports sRGB color space by default. For proper color management:
- ICC profiles from JXL files are extracted but reported as sRGB
- Future versions should properly embed ICC profile data

## Limitations

- Static images only (no animation support currently)
- BGRA8 pixel format output (ImageGlass native format)
- No encoding support (decoder only)

## References

- [ImageGlass Plugin Development Guide](https://imageglass.org/docs/codec-plugins)
- [jxl-rs Documentation](https://docs.rs/jxl/latest/jxl/)
- [ImageGlass SDK GitHub](https://github.com/ImageGlass/SDK)
- [JPEG XL Specification](https://jpegxl.org/)

## License

BSD-3-Clause (same as jxl-rs)