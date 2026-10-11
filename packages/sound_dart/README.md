# `sound_dart`

Cross-platform sound playback for Dart and Flutter with the goal of no system 
dependencies that need to be installed.  Pure Dart (works in CLI tools) with 
native audio over FFI.

For Flutter apps, see the `sound_flutter` package, which builds the native 
library for you via cargokit and should 'just work'.

The only build-time requirement beyond Dart/Flutter is Rust.  Precompiled
binaries are planned to remove that.

```dart
import 'package:sound_dart/sound_dart.dart';

final playback = await Sound.playFile('chime.wav', volume: 0.8);
await playback.onComplete;
await playback.dispose();
```

## Native library (CLI use)

Outside Flutter, nothing builds the native library for you. Build the
`sound_cli` crate from [the repository](https://github.com/ManyMath/sound):

```sh
cargo build --release --manifest-path packages/sound_flutter/rust/Cargo.toml
```

and point `SOUND_DART_LIB` at the result (`libsound_cli.so`,
`libsound_cli.dylib`, or `sound_cli.dll` under
`packages/sound_flutter/rust/target/release/`). Running from inside the
repository finds it automatically. Without the library, `Sound` falls back to
a silent backend.
