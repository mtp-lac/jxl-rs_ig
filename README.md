# JPEG XL Codec Plugin for ImageGlass

This plugin implements a native codec for ImageGlass 10 that adds support for JPEG XL (JXL) image files using the official [jxl-rs](https://github.com/libjxl/jxl-rs) Rust library.

## Features

- Decodes static JPEG XL images (.jxl extension)
- Extracts image metadata (dimensions, color space, alpha channel)
- Reports frame count for static images
- Supports cancellation during decode operations

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
  "version": "1.0.0",
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