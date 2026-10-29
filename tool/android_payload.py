"""Preflight and identify source-built Android APKs without installing SDKs."""
import hashlib
import json
import os
from pathlib import Path
import zipfile


def prepare(config, repo, output, run, environ=None):
    env = dict(os.environ if environ is None else environ)
    settings = config['android']
    sdk_value = env.get('ANDROID_SDK_ROOT') or env.get('ANDROID_HOME')
    if not sdk_value:
        raise RuntimeError('Set ANDROID_SDK_ROOT to an already provisioned Android SDK')
    sdk = Path(sdk_value).resolve()
    if env.get('ANDROID_HOME') and Path(env['ANDROID_HOME']).resolve() != sdk:
        raise RuntimeError('ANDROID_HOME and ANDROID_SDK_ROOT disagree')
    suffix = '.exe' if os.name == 'nt' else ''
    required = [sdk / 'platforms' / settings['platform'] / 'android.jar',
                sdk / 'build-tools' / settings['build_tools'] / ('aapt2' + suffix),
                sdk / 'ndk' / settings['ndk'] / 'source.properties']
    if settings.get('cmake'):
        required.append(sdk / 'cmake' / settings['cmake'] / 'bin' / ('cmake' + suffix))
    missing = [str(path) for path in required if not path.is_file()]
    if missing:
        raise RuntimeError('Provision Android tools before verification; no installer fallback: ' + ', '.join(missing))
    java_home = env.get('JAVA_HOME')
    if not java_home or not (Path(java_home) / 'bin' / ('javac' + suffix)).is_file():
        raise RuntimeError('Set JAVA_HOME to an already provisioned JDK with javac; no installer fallback')
    env['PATH'] = str(Path(java_home) / 'bin') + os.pathsep + env.get('PATH', '')
    java_version = run(['javac', '-version'], repo, output / 'java-version.log', env=env).strip()
    channel = settings['rust_toolchain']
    installed = run(['rustup', 'toolchain', 'list'], repo, output / 'rust-toolchains.log').splitlines()
    if not any(line.split() and (line.split()[0] == channel or line.split()[0].startswith(channel + '-')) for line in installed):
        raise RuntimeError(f'Provision Rust toolchain {channel}; no installer fallback')
    targets = run(['rustup', 'target', 'list', '--installed', '--toolchain', channel], repo, output / 'rust-targets.log').splitlines()
    required_targets = config['rust_targets']['apk']
    if not required_targets or not set(required_targets).issubset(targets):
        raise RuntimeError('Provision all configured Android Rust targets; no installer fallback')
    cargo = Path(run(['rustup', 'which', 'cargo', '--toolchain', channel], repo, output / 'rust-which-cargo.log').strip())
    if not cargo.is_file():
        raise RuntimeError('Selected Rust toolchain cargo is missing; no installer fallback')
    # A real cargo/rustc earlier on PATH bypasses rustup and RUSTUP_TOOLCHAIN.
    # Select actual binaries in this child environment, leaving the host untouched.
    env['PATH'] = str(cargo.parent) + os.pathsep + env.get('PATH', '')
    env.update(ANDROID_SDK_ROOT=str(sdk), ANDROID_HOME=str(sdk), RUSTUP_TOOLCHAIN=channel)
    compiler = run(['rustc', '--version', '--verbose'], repo, output / 'native-rust-version.log', env=env)
    cargo_version = run(['cargo', '--version'], repo, output / 'native-cargo-version.log', env=env)
    cargo_ndk = None
    if settings.get('cargo_ndk'):
        cargo_ndk = run(['cargo', 'ndk', '--version'], repo, output / 'cargo-ndk-version.log', env=env).strip()
    env.setdefault('GRADLE_USER_HOME', str(output / 'gradle-home'))
    env.setdefault('ANDROID_USER_HOME', str(output / 'android-home'))
    record = {'sdk': str(sdk), 'ndk': settings['ndk'], 'platform': settings['platform'],
              'build_tools': settings['build_tools'], 'rust_toolchain': channel,
              'rust_compiler': compiler.strip(), 'cargo': str(cargo), 'cargo_version': cargo_version.strip(),
              'cargo_ndk_version': cargo_ndk, 'rust_targets': required_targets,
              'java_home': java_home, 'java_compiler': java_version,
              'gradle_home': env['GRADLE_USER_HOME'], 'android_user_home': env['ANDROID_USER_HOME'],
              'sdk_auto_installation': False}
    (output / 'android-toolchain.json').write_bytes((json.dumps(record, indent=2) + '\n').encode())
    return env


def configure_consumer(consumer, config, env, output, run):
    android = consumer / 'android'
    properties = android / 'gradle.properties'
    # These are consumer-local settings; never modify installed SDKs or user settings.
    with properties.open('a', encoding='utf-8', newline='\n') as stream:
        stream.write('\nandroid.builder.sdkDownload=false\norg.gradle.java.installations.auto-download=false\norg.gradle.workers.max=2\norg.gradle.daemon=false\norg.gradle.jvmargs=-Xmx2G\nkotlin.compiler.execution.strategy=in-process\n')
    # Check the actual app/plugin NDK chosen by Gradle, not just a directory that exists.
    expected = json.dumps(config['android']['ndk'])
    script = android / 'payload-toolchain.gradle'
    script.write_bytes(('''allprojects {
    afterEvaluate {
        if (plugins.hasPlugin('com.android.application') || plugins.hasPlugin('com.android.library')) {
            def androidExtension = extensions.getByName('android')
            if (androidExtension.ndkVersion != EXPECTED) {
                throw new GradleException("Unexpected NDK for ${path}: ${androidExtension.ndkVersion}")
            }
            println("PAYLOAD_ANDROID_TOOLCHAIN project=${path} ndk=${androidExtension.ndkVersion} directory=${androidExtension.ndkDirectory}")
        }
    }
}
'''.replace('EXPECTED', expected)).encode())
    wrapper = android / ('gradlew.bat' if os.name == 'nt' else 'gradlew')
    run([str(wrapper), '--no-daemon', '--init-script', str(script), 'help'],
        android, output / 'android-gradle-preflight.log', env=env)


def artifact(consumer, config):
    apk = consumer / 'build/app/outputs/flutter-apk/app-debug.apk'
    expected = {f'lib/{abi}/{config["android"]["library"]}' for abi in config['android']['apk_abis']}
    if not expected:
        raise RuntimeError("APK native ABI contract must not be empty")
    with zipfile.ZipFile(apk) as archive:
        actual = {name for name in archive.namelist() if name.startswith('lib/') and name.endswith('/' + config['android']['library'])}
        if actual != expected:
            raise RuntimeError(f'APK native ABI closure differs: expected {sorted(expected)}, got {sorted(actual)}')
        hashes = {name: hashlib.sha256(archive.read(name)).hexdigest() for name in sorted(actual)}
    return {'path': apk.relative_to(consumer).as_posix(),
            'sha256': hashlib.sha256(apk.read_bytes()).hexdigest(), 'native_libraries': hashes,
            'scope': 'Android debug APK build and packaged native ABI identities; no device or hardware runtime claim'}
