# sound_flutter

Flutter integration for [`sound_dart`](https://pub.dev/packages/sound_dart):
cross-platform audio with no system dependencies for users or devs to install.

This package builds the `sound_cli` Rust crate it ships in `rust/`
automatically (via [cargokit](https://github.com/ManyMath/cargokit)) and
bundles it with your app, then re-exports the full `sound_dart` API.

```dart
import 'package:sound_flutter/sound_flutter.dart';

await SoundFlutter.ensureInitialized();
final playback = await Sound.playFile('/path/to/chime.wav');
await playback.onComplete;
```

On-device playback tests live in `example/integration_test`. Run them against
a connected device, emulator or desktop target:

```sh
cd example
flutter test integration_test -d <device-id>
```

Building requires Rust (`rustup`). To remove even that for consumers, the repo
is set up for cargokit precompiled binaries:

- `rust/cargokit.yaml` holds the public signing key and the
  release URL prefix.
- `.github/workflows/precompile.yml` builds and uploads signed binaries to
  `precompiled_<crate-hash>` releases on tag pushes. It needs a
  `CARGOKIT_PRIVATE_KEY` repository secret: the hex private key printed by
  `dart run build_tool gen-key` (keep it secret; never commit it).

cargokit only uses a prebuilt binary when Rust isn't installed (or the app
opts in with `use_precompiled_binaries: true` in its `cargokit_options.yaml`)
and a signed release for the current crate exists; otherwise it builds from
source. The workflow publishes binaries for Linux x86_64, Android, macOS,
iOS (device and simulator) and Windows x86_64.

## Requirements

- Android: playback uses AAudio, available from Android 8.0 (API 26). The
  plugin builds with Flutter's default `minSdk`; on older devices playback
  falls back to the silent backend.
- iOS 13 or later; macOS 10.15 or later.
