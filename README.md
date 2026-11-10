# JPEG XL Codec Plugin for ImageGlass

This plugin implements a native codec for ImageGlass 10 that adds support for JPEG XL (JXL) image files using the official [jxl-rs](https://github.com/libjxl/jxl-rs) Rust library.

## Features

- Decodes static JPEG XL images (.jxl extension)
- Extracts image metadata (dimensions, color space, alpha channel)
- Reports frame count for static images
- Supports cancellation during metadata and decode operations (polled at every
  input chunk, so long decodes can be aborted)
- Streaming input: metadata loads only read the file header; decodes read the
  file incrementally, keeping peak memory close to the pixel buffer
- Bounded, de-duplicated ICC profile cache — browsing thousands of files does
  not accumulate memory
- Size guards: frames above 32000×32000 are refused, and huge allocations are
  backed by an available-physical-memory check before `try_reserve` (OOM is
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
  "version": "1.2.0",
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
3. **`can_handle_extension`** - Matches .jxl extension
4. **`load_metadata`** - Extracts image info using jxl-rs decoder
5. **`decode_static_raster`** - Decodes image to BGRA8 pixel format
6. **`free_pixel_buffer`** - Releases allocated pixel memory

Both `load_metadata` and `decode_static_raster` feed the `jxl-rs` decoder from
an incremental file reader: a metadata call stops as soon as the image headers
are parsed (it never reads the pixel payload), and a decode reads one chunk at a
time. Each chunk boundary is also a cancellation poll, so the host's
`IsCancellationRequested` is honored during long work. ICC profiles returned
through `IGImageInfo.IccProfileData` are kept in a de-duplicated, bounded store
(identical profiles share one allocation; the store caps out at 64 profiles /
4 MiB with FIFO eviction) — the ABI only requires the host to read the profile
synchronously inside `LoadMetadata`.

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