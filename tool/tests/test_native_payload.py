import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from subprocess import CompletedProcess

spec = importlib.util.spec_from_file_location("native_payload", Path(__file__).parents[1] / "native_payload.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)
FIXTURES = Path(__file__).parent / "fixtures"


class PayloadTests(unittest.TestCase):
    def test_debug_macos_preflights_only_selected_host_architecture(self):
        config = {"rust_targets": {"macos": ["{host_arch}-apple-darwin"], "windows": ["{host_arch}-pc-windows-msvc"]}}
        self.assertEqual(gate.required_rust_targets(config, "macos", "arm64"), ["aarch64-apple-darwin"])
        self.assertEqual(gate.required_rust_targets(config, "macos", "x86_64"), ["x86_64-apple-darwin"])
        self.assertEqual(gate.required_rust_targets(config, "windows", "AMD64"), ["x86_64-pc-windows-msvc"])
        with self.assertRaises(RuntimeError):
            gate.required_rust_targets(config, "macos", "unknown")
        with self.assertRaises(RuntimeError):
            gate.required_rust_targets(config, "ios", "arm64")

    def test_resolves_sdk_batch_entrypoint(self):
        with tempfile.TemporaryDirectory() as tmp:
            with patch.object(gate.shutil, "which", return_value="C:/SDK/bin/dart.bat"), patch.object(gate.subprocess, "run", return_value=CompletedProcess([], 0, "ok")) as invoke:
                gate.run(["dart", "--version"], tmp, Path(tmp) / "version.log")
            self.assertEqual(invoke.call_args.args[0], ["C:/SDK/bin/dart.bat", "--version"])
            self.assertFalse(invoke.call_args.kwargs.get("shell", False))

    def test_windows_children_use_real_toolchain_binaries(self):
        from types import SimpleNamespace
        with tempfile.TemporaryDirectory() as tmp:
            with patch.object(gate, "os", SimpleNamespace(name="nt", environ=gate.os.environ)), patch.object(gate.shutil, "which", return_value="C:/SDK/bin/dart.bat"), patch.object(gate.subprocess, "run", return_value=CompletedProcess([], 0, "ok")) as invoke:
                gate.run(["dart", "--version"], tmp, Path(tmp) / "version.log", {"PATH": "C:/SDK/bin"})
            self.assertEqual(invoke.call_args.kwargs["env"]["RUSTUP_WINDOWS_PATH_ADD_BIN"], "1")

    def test_explicit_empty_child_environment_is_preserved(self):
        with tempfile.TemporaryDirectory() as tmp:
            with patch.object(gate.shutil, "which", return_value="/sdk/dart"), patch.object(gate.subprocess, "run", return_value=CompletedProcess([], 0, "ok")) as invoke:
                gate.run(["dart", "--version"], tmp, Path(tmp) / "version.log", {})
            self.assertNotIn("PATH", invoke.call_args.kwargs["env"])

    def test_runtime_library_keeps_legacy_linux_config(self):
        self.assertEqual(gate.runtime_library({"library_name": "libdemo.so"}, "linux"),
                         {"name": "libdemo.so", "bundle_dir": "bundle/lib"})
        self.assertIsNone(gate.runtime_library({"library_name": "libdemo.so"}, "windows"))

    def test_runtime_library_per_target(self):
        config = {"runtime_libraries": {"windows": {"name": "demo.dll", "bundle_dir": "runner/Debug"}}}
        self.assertEqual(gate.runtime_library(config, "windows")["name"], "demo.dll")
        self.assertIsNone(gate.runtime_library(config, "apk"))

    def test_bundled_library_matches_only_the_bundle_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            consumer = Path(tmp)
            for directory in ["build/windows/x64/runner/Debug", "build/windows/x64/plugins/demo/Debug"]:
                (consumer / directory).mkdir(parents=True)
                (consumer / directory / "demo.dll").write_bytes(b"dll")
            library = {"name": "demo.dll", "bundle_dir": "runner/Debug"}
            with patch.object(gate.shutil, "which", return_value=None):
                found = gate.bundled_library(consumer, "windows", library)
            self.assertEqual(found, consumer / "build/windows/x64/runner/Debug/demo.dll")
            (consumer / "build/windows/x64/runner/Debug/copy").mkdir()
            (consumer / "build/windows/x64/runner/Debug/copy/demo.dll").write_bytes(b"dll")
            (consumer / "build/windows/arm64/runner/Debug").mkdir(parents=True)
            (consumer / "build/windows/arm64/runner/Debug/demo.dll").write_bytes(b"dll")
            with self.assertRaisesRegex(RuntimeError, "exactly one"):
                gate.bundled_library(consumer, "windows", library)

    def test_windows_refuses_same_named_library_on_path(self):
        from types import SimpleNamespace
        with tempfile.TemporaryDirectory() as tmp:
            consumer = Path(tmp)
            (consumer / "build/windows/x64/runner/Debug").mkdir(parents=True)
            (consumer / "build/windows/x64/runner/Debug/demo.dll").write_bytes(b"dll")
            with patch.object(gate, "os", SimpleNamespace(name="nt", environ={"PATH": "C:/elsewhere"})), patch.object(gate.shutil, "which", return_value="C:/elsewhere/demo.dll"):
                with self.assertRaisesRegex(RuntimeError, "on PATH"):
                    gate.bundled_library(consumer, "windows", {"name": "demo.dll", "bundle_dir": "runner/Debug"})

    def test_missing_native_source(self):
        with self.assertRaisesRegex(ValueError, "Missing published source"):
            gate.inspect_package(FIXTURES / "missing-source", {"required": ["rust/Cargo.toml"]})

    def test_wrong_flutter_target(self):
        with self.assertRaisesRegex(ValueError, "Missing platform contract"):
            gate.inspect_package(FIXTURES / "wrong-target", {"contracts": {"linux/CMakeLists.txt": 'set(PLUGIN_NAME "glow_plugin")'}})

    def test_escaping_cargo_dependency(self):
        with self.assertRaisesRegex(ValueError, "escapes package"):
            gate.inspect_package(FIXTURES / "escaped-dependency", {})

    def test_internal_dependency_must_exist(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "Cargo.toml").write_text('[dependencies]\ncore = {path = "core"}\n')
            with self.assertRaisesRegex(ValueError, "Missing Cargo path"):
                gate.inspect_package(root, {})
            (root / "core").mkdir()
            (root / "core/Cargo.toml").write_text('[package]\nname="core"\nversion="0.0.1"\n')
            gate.inspect_package(root, {})

    def test_candidate_constraints_cannot_be_hidden_by_overrides(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name in ["payload_core", "payload_shim"]:
                package = root / "packages" / name
                package.mkdir(parents=True)
                manifest = f"name: {name}\nversion: 0.0.2\nenvironment:\n  sdk: ^3.10.0\n"
                if name == "payload_shim":
                    manifest += "dependencies:\n  payload_core: ^1.0.0\n"
                (package / "pubspec.yaml").write_text(manifest)
            with self.assertRaises(RuntimeError):
                gate.verify_candidate_constraints(root, [{"name": "payload_core"}, {"name": "payload_shim"}])
            self.assertIn("does not satisfy declared constraint", (root / "candidate-constraints.log").read_text())

    def test_pubignore_can_include_gitignored_generated_source(self):
        import subprocess
        import tarfile
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            (repo / ".gitignore").write_text("required.txt\n")
            (repo / ".pubignore").write_text("")
            (repo / "required.txt").write_text("generated native source")
            (repo / "pubspec.yaml").write_text("name: payload_fixture\nversion: 0.0.1\nrepository: https://example.com/fixture\ndescription: Local payload selection regression fixture.\nenvironment:\n  sdk: ^3.10.0\n")
            for name in ["LICENSE", "README.md", "CHANGELOG.md"]:
                (repo / name).write_text("Fixture only\n")
            stage = root / "stage"
            gate.snapshot(repo, stage)
            archive = root / "fixture.tar.gz"
            gate.run(["dart", "pub", "publish", f"--to-archive={archive}"], stage, root / "archive.log")
            with tarfile.open(archive) as payload:
                self.assertEqual(payload.extractfile("required.txt").read(), b"generated native source")

    def test_modified_staged_source_is_rejected(self):
        import json
        import hashlib
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / 'source.rs'
            source.write_text('original')
            (root / 'provenance.json').write_text(json.dumps({'files_sha256': {'source.rs': hashlib.sha256(source.read_bytes()).hexdigest()}}))
            gate.inspect_package(root, {'provenance': ['provenance.json']})
            source.write_text('stale edit')
            with self.assertRaisesRegex(ValueError, 'Stale staged source'):
                gate.inspect_package(root, {'provenance': ['provenance.json']})

    def test_archive_cannot_escape(self):
        import io
        import tarfile
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            archive = root / "bad.tar.gz"
            with tarfile.open(archive, "w:gz") as target:
                member = tarfile.TarInfo("../escaped")
                member.size = 1
                target.addfile(member, io.BytesIO(b"x"))
            with self.assertRaisesRegex(ValueError, "escapes package"):
                gate.extract(archive, root / "extracted")
            self.assertFalse((root / "escaped").exists())


if __name__ == "__main__":
    unittest.main()
