# sound

Audio playback for Dart and Flutter across platforms. Native builds
require Rust in addition to Dart or Flutter. Precompiled binaries are planned
to remove that build requirement.

This is a [melos](https://melos.invertase.dev) monorepo:

| Package | What it is |
|---|---|
| [`packages/sound_dart`](packages/sound_dart) | Pure Dart (no Flutter); works in CLI tools. Reaches native audio over `dart:ffi`, with a pluggable backend registry. |
| [`packages/sound_flutter`](packages/sound_flutter) | Flutter plugin. Builds the native library automatically (via cargokit) and re-exports the `sound_dart` API. |
| [`packages/sound_flutter/rust`](packages/sound_flutter/rust) | The `sound_cli` Rust crate (the C ABI behind the FFI backend), kept inside `sound_flutter` so it ships with that package. Decodes WAV/MP3/OGG/FLAC and plays with no system deps - loads the OS audio library at runtime: `libasound.so.2` on Linux, `libaaudio.so` on Android, AudioToolbox on macOS/iOS, `winmm.dll` on Windows. |

## Quick start (Dart CLI)

```dart
import 'package:sound_dart/sound_dart.dart';

final playback = await Sound.playFile('chime.wav', volume: 0.8);
await playback.onComplete;
```

Build the native library first: `cargo build --manifest-path packages/sound_flutter/rust/Cargo.toml`.

## Status

| Host | Status |
|---|---|
| Android | Working |
| Android Emulator | Working |
| iOS | Working |
| iOS Simulator | Working |
| macOS | Working |
| Ubuntu 24.04 | Working |
| Web | Working |
| Windows 11 | Working |

Android Emulator tested on macOS, Ubuntu 24.04, and Windows 11.
Web tested on macOS in Chrome and Safari, Ubuntu 24.04 in Chrome and Firefox, and Windows 11 in Chrome and Edge.

## Development

Driven by [melos](https://melos.invertase.dev) (config lives in the root
`pubspec.yaml`). After `dart pub get` (or `flutter pub get`):

```sh
dart run melos run native:build   # build the Rust shared library
dart run melos run analyze        # analyze sound_dart + sound_flutter
dart run melos run format         # check Dart formatting (format:fix to apply)
dart run melos run test           # Dart + Flutter tests
dart run melos run native:test    # Rust crate tests
```
