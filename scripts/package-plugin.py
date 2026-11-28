#!/usr/bin/env python3
"""Package ImageGlass JPEG XL codec plugin binaries into .igplugin.zip archives."""

from __future__ import annotations

import argparse
import json
from dataclasses import dataclass
from pathlib import Path
import zipfile


@dataclass(frozen=True)
class PlatformSpec:
    input_name: str
    executable_name: str


PLATFORM_SPECS: dict[str, PlatformSpec] = {
    "win-x64": PlatformSpec("jxl_codec.dll", "JxlCodec.dll"),
    "linux-x64": PlatformSpec("libjxl_codec.so", "libjxl_codec.so"),
    "osx-arm64": PlatformSpec("libjxl_codec.dylib", "libjxl_codec.dylib"),
}

NOTICE_FILES = ("LICENSE", "THIRD_PARTY_NOTICES.txt")


def package_platform(
    platform: str,
    binary_path: Path,
    repo_root: Path,
    out_dir: Path,
) -> Path:
    """Create `<out_dir>/jxl-codec_<platform>.igplugin.zip` for the given binary."""
    if platform not in PLATFORM_SPECS:
        supported = ", ".join(sorted(PLATFORM_SPECS))
        raise ValueError(f"Unsupported platform '{platform}' (expected one of: {supported})")

    if not binary_path.is_file():
        raise FileNotFoundError(f"Plugin binary not found: {binary_path}")

    manifest_path = repo_root / "igplugin.json"
    if not manifest_path.is_file():
        raise FileNotFoundError(f"Manifest not found: {manifest_path}")

    for notice in NOTICE_FILES:
        notice_path = repo_root / notice
        if not notice_path.is_file():
            raise FileNotFoundError(f"Required notice file not found: {notice_path}")

    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    plugin_id = manifest.get("id")
    if not isinstance(plugin_id, str) or not plugin_id.strip():
        raise ValueError("igplugin.json must contain a non-empty 'id' field")

    target_executable = PLATFORM_SPECS[platform].executable_name
    manifest["executable"] = target_executable

    out_dir.mkdir(parents=True, exist_ok=True)
    archive_path = out_dir / f"jxl-codec_{platform}.igplugin.zip"

    with zipfile.ZipFile(archive_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        zf.writestr(
            f"{plugin_id}/igplugin.json",
            json.dumps(manifest, indent=2) + "\n",
        )
        zf.write(binary_path, arcname=f"{plugin_id}/{target_executable}")
        for notice in NOTICE_FILES:
            zf.write(repo_root / notice, arcname=f"{plugin_id}/{notice}")

    return archive_path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Package ImageGlass .igplugin.zip archives."
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="Repository root containing igplugin.json, LICENSE, and THIRD_PARTY_NOTICES.txt.",
    )
    parser.add_argument(
        "--out-dir",
        type=Path,
        default=Path("dist"),
        help="Directory where .igplugin.zip archives will be written.",
    )
    parser.add_argument(
        "--artifacts-dir",
        type=Path,
        help="Directory containing all platform binaries (packages all platforms).",
    )
    parser.add_argument(
        "--platform",
        choices=sorted(PLATFORM_SPECS),
        help="Single target platform to package.",
    )
    parser.add_argument(
        "--binary",
        type=Path,
        help="Path to compiled shared library when using --platform.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.artifacts_dir is not None:
        if args.platform is not None or args.binary is not None:
            raise SystemExit("Use either --artifacts-dir or --platform/--binary, not both.")
        targets = [
            (platform, args.artifacts_dir / spec.input_name)
            for platform, spec in PLATFORM_SPECS.items()
        ]
    elif args.platform is not None and args.binary is not None:
        targets = [(args.platform, args.binary)]
    else:
        raise SystemExit("Provide either --artifacts-dir or both --platform and --binary.")

    for platform, binary_path in targets:
        archive = package_platform(
            platform=platform,
            binary_path=binary_path,
            repo_root=args.repo_root,
            out_dir=args.out_dir,
        )
        print(f"Packaged {archive}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
