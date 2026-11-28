# JPEG XL Codec for ImageGlass

Open JPEG XL (`.jxl`) images in ImageGlass 10 with this codec plugin.

## Install

1. Download the `.igplugin.zip` package for your platform from [Releases](../../releases).
2. In ImageGlass, open **Settings > Plugins** and click **Add**.
3. Select the downloaded package, then choose **Trust and enable**.

## Features and limitations

- Opens JPEG XL images and displays image details and thumbnails.
- Uses multiple CPU cores to decode images.
- Supports Windows x64, Linux x64, and macOS ARM64 packages.
- Animated JPEG XL images and CMYK images are not currently supported.

## Build from source

Install the stable [Rust toolchain](https://www.rust-lang.org/tools/install), then build and test:

```sh
cargo build --release
cargo test --locked
```

To create an installable package, run the packaging script with the platform and built library. For example, on Windows x64:

```sh
python scripts/package-plugin.py --platform win-x64 --binary target/release/jxl_codec.dll
```

For Linux x64, use `--platform linux-x64` with `target/release/libjxl_codec.so`. For macOS ARM64, use `--platform osx-arm64` with `target/release/libjxl_codec.dylib`.

## License

This project is licensed under the BSD-3-Clause license. See [LICENSE](LICENSE) and [third-party notices](THIRD_PARTY_NOTICES.txt).

## Links

- [ImageGlass plugin guide](https://imageglass.org/docs/codec-plugins)
- [jxl-rs](https://github.com/libjxl/jxl-rs)
- [JPEG XL](https://jpegxl.org/)
