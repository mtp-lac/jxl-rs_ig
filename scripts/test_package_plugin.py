import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
import zipfile


SCRIPT_PATH = Path(__file__).resolve().parent / "package-plugin.py"


def load_module():
    spec = importlib.util.spec_from_file_location("package_plugin", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class PackagePluginTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        (self.root / "igplugin.json").write_text(
            json.dumps(
                {
                    "id": "Plugin_JxlCodec",
                    "name": "JPEG XL Codec",
                    "version": "3.0.0",
                    "executable": "JxlCodec.dll",
                }
            ),
            encoding="utf-8",
        )
        (self.root / "LICENSE").write_text("BSD-3-Clause", encoding="utf-8")
        (self.root / "THIRD_PARTY_NOTICES.txt").write_text("Notices", encoding="utf-8")
        self.out_dir = self.root / "dist"

    def tearDown(self):
        self.tmp.cleanup()

    def test_packages_windows_plugin_with_renamed_dll(self):
        mod = load_module()
        bin_path = self.root / "jxl_codec.dll"
        bin_path.write_bytes(b"MZ-fake-dll")

        archive = mod.package_platform(
            platform="win-x64",
            binary_path=bin_path,
            repo_root=self.root,
            out_dir=self.out_dir,
        )

        self.assertEqual(archive.name, "jxl-codec_win-x64.igplugin.zip")
        with zipfile.ZipFile(archive, "r") as zf:
            names = set(zf.namelist())
            self.assertEqual(
                names,
                {
                    "Plugin_JxlCodec/igplugin.json",
                    "Plugin_JxlCodec/JxlCodec.dll",
                    "Plugin_JxlCodec/LICENSE",
                    "Plugin_JxlCodec/THIRD_PARTY_NOTICES.txt",
                },
            )
            manifest = json.loads(zf.read("Plugin_JxlCodec/igplugin.json"))
            self.assertEqual(manifest["executable"], "JxlCodec.dll")
            self.assertEqual(zf.read("Plugin_JxlCodec/JxlCodec.dll"), b"MZ-fake-dll")

    def test_packages_linux_and_macos_with_matching_executable_in_manifest(self):
        mod = load_module()
        cases = [
            ("linux-x64", "libjxl_codec.so", b"ELF-fake"),
            ("osx-arm64", "libjxl_codec.dylib", b"MACH-fake"),
        ]
        for platform, exe_name, payload in cases:
            bin_path = self.root / exe_name
            bin_path.write_bytes(payload)

            archive = mod.package_platform(
                platform=platform,
                binary_path=bin_path,
                repo_root=self.root,
                out_dir=self.out_dir,
            )

            self.assertEqual(archive.name, f"jxl-codec_{platform}.igplugin.zip")
            with zipfile.ZipFile(archive, "r") as zf:
                names = set(zf.namelist())
                self.assertIn(f"Plugin_JxlCodec/{exe_name}", names)
                manifest = json.loads(zf.read("Plugin_JxlCodec/igplugin.json"))
                self.assertEqual(manifest["executable"], exe_name)
                self.assertEqual(zf.read(f"Plugin_JxlCodec/{exe_name}"), payload)

    def test_raises_when_binary_missing(self):
        mod = load_module()
        with self.assertRaises(FileNotFoundError):
            mod.package_platform(
                platform="win-x64",
                binary_path=self.root / "missing.dll",
                repo_root=self.root,
                out_dir=self.out_dir,
            )


if __name__ == "__main__":
    unittest.main()
