/// Cross-platform sound playback for Dart and Flutter.
///
/// `sound_dart` is pure Dart and has no Flutter dependency, so it works in
/// CLI tools as well as Flutter apps. Native playback is reached over FFI;
/// see [SoundBackend] for the pluggable backend contract and [Sound] for
/// the entry point.
library;

export 'src/backends/silent_backend.dart';
export 'src/exceptions.dart';
export 'src/fade.dart';
export 'src/pcm.dart';
export 'src/pcm_tap.dart';
export 'src/playback.dart';
export 'src/sound.dart';
export 'src/sound_backend.dart';
export 'src/sound_source.dart';
