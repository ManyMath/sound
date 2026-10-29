import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import zipfile

spec = importlib.util.spec_from_file_location('android_payload', Path(__file__).parents[1] / 'android_payload.py')
android = importlib.util.module_from_spec(spec)
spec.loader.exec_module(android)


class AndroidTests(unittest.TestCase):
    def config(self):
        return {'android': {'rust_toolchain': 'stable', 'ndk': '28.2.13676358',
                           'platform': 'android-36', 'build_tools': '36.0.0',
                           'library': 'libfixture.so', 'apk_abis': ['arm64-v8a', 'x86_64']},
                'rust_targets': {'apk': ['aarch64-linux-android', 'x86_64-linux-android']}}

    def sdk(self, root):
        jdk = root.parent / 'jdk' / 'bin'
        jdk.mkdir(parents=True, exist_ok=True)
        (jdk / ('javac.exe' if android.os.name == 'nt' else 'javac')).write_bytes(b'fixture')
        for name in ['platforms/android-36/android.jar', 'build-tools/36.0.0/aapt2' + ('.exe' if android.os.name == 'nt' else ''),
                     'ndk/28.2.13676358/source.properties']:
            p = root / name
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(b'fixture')

    def run_tool(self, args, *unused, **kwargs):
        if args == ['javac', '-version']:
            return 'javac fixture 21\n'
        if args[:3] == ['rustup', 'toolchain', 'list']:
            return 'stable-x86_64-unknown-linux-gnu\n'
        if args[:3] == ['rustup', 'target', 'list']:
            return 'aarch64-linux-android\nx86_64-linux-android\n'
        if args[:3] == ['rustup', 'which', 'cargo']:
            cargo = Path(unused[0]) / 'stable-bin' / 'cargo'
            cargo.parent.mkdir(exist_ok=True)
            cargo.write_bytes(b'fixture')
            return str(cargo) + '\n'
        if args[0] in ('rustc', 'cargo'):
            self.assertEqual(Path(kwargs['env']['PATH'].split(android.os.pathsep)[0]).name, 'stable-bin')
            return 'explicit stable toolchain fixture\n'
        self.fail(f'Unexpected command (especially no installer): {args}')

    def test_selected_toolchain_and_task_homes_are_child_scoped(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); self.sdk(root / 'sdk')
            original = {'JAVA_HOME': str(root / 'jdk'), 'ANDROID_SDK_ROOT': str(root / 'sdk'), 'RUSTUP_TOOLCHAIN': '1.94.0', 'PATH': str(root / 'wrong-real-1.94/bin')}
            env = android.prepare(self.config(), root, root, self.run_tool, original)
            self.assertEqual(original['RUSTUP_TOOLCHAIN'], '1.94.0')
            self.assertEqual(original['PATH'], str(root / 'wrong-real-1.94/bin'))
            self.assertEqual(Path(env['PATH'].split(android.os.pathsep)[0]), root / 'stable-bin')
            self.assertEqual(env['RUSTUP_TOOLCHAIN'], 'stable')
            self.assertEqual(env['GRADLE_USER_HOME'], str(root / 'gradle-home'))
            self.assertFalse(json.loads((root / 'android-toolchain.json').read_text())['sdk_auto_installation'])

    def test_missing_sdk_and_target_fail_before_installation(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.assertRaisesRegex(RuntimeError, 'no installer'):
                android.prepare(self.config(), root, root, self.run_tool, {'JAVA_HOME': str(root / 'jdk'), 'ANDROID_SDK_ROOT': str(root / 'sdk')})
            self.sdk(root / 'sdk')
            def missing_target(args, *a, **kw):
                return '' if args[:3] == ['rustup', 'target', 'list'] else self.run_tool(args, *a, **kw)
            with self.assertRaisesRegex(RuntimeError, 'targets'):
                android.prepare(self.config(), root, root, missing_target, {'JAVA_HOME': str(root / 'jdk'), 'ANDROID_SDK_ROOT': str(root / 'sdk')})

    def test_required_cargo_ndk_fails_during_preflight(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); self.sdk(root / 'sdk')
            config = self.config(); config['android']['cargo_ndk'] = True
            def missing_ndk(args, *a, **kw):
                if args[:2] == ['cargo', 'ndk']:
                    raise RuntimeError('cargo-ndk is missing')
                return self.run_tool(args, *a, **kw)
            with self.assertRaisesRegex(RuntimeError, 'cargo-ndk'):
                android.prepare(config, root, root, missing_ndk, {'JAVA_HOME': str(root / 'jdk'), 'ANDROID_SDK_ROOT': str(root / 'sdk')})

    def test_conflicting_sdk_roots_fail(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.assertRaisesRegex(RuntimeError, 'disagree'):
                android.prepare(self.config(), root, root, self.run_tool, {'ANDROID_SDK_ROOT': str(root / 'a'), 'ANDROID_HOME': str(root / 'b')})

    def test_consumer_blocks_sdk_downloads_and_checks_actual_ndk(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); (root / 'android').mkdir()
            calls = []
            android.configure_consumer(root, self.config(), {'MARKER': 'child'}, root, lambda *a, **kw: calls.append((a, kw)))
            self.assertIn('android.builder.sdkDownload=false', (root / 'android/gradle.properties').read_text())
            self.assertIn('28.2.13676358', (root / 'android/payload-toolchain.gradle').read_text())
            self.assertIn('--init-script', calls[0][0][0])
            self.assertEqual(calls[0][1]['env'], {'MARKER': 'child'})

    def test_apk_missing_native_abi_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); apk = root / 'build/app/outputs/flutter-apk/app-debug.apk'
            apk.parent.mkdir(parents=True)
            with zipfile.ZipFile(apk, 'w') as archive:
                archive.writestr('lib/arm64-v8a/libfixture.so', b'native fixture')
            with self.assertRaisesRegex(RuntimeError, 'ABI closure'):
                android.artifact(root, self.config())
            with zipfile.ZipFile(apk, 'a') as archive:
                archive.writestr('lib/x86_64/libfixture.so', b'native fixture x64')
            self.assertEqual(len(android.artifact(root, self.config())['native_libraries']), 2)
