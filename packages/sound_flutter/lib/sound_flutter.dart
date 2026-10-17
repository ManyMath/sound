/// Flutter integration for the `sound_dart` package.
///
/// This package contributes the native `sound_cli` library to your Flutter
/// app (built automatically via cargokit) and re-exports the full
/// `sound_dart` API.
/// In most cases you only need the re-exported [Sound] entry point:
///
/// ```dart
/// import 'package:sound_flutter/sound_flutter.dart';
///
/// await SoundFlutter.ensureInitialized();
/// final playback = await Sound.playFile('/path/to/chime.wav');
/// ```
library;

import 'package:flutter/services.dart' show AssetBundle, rootBundle;
import 'package:sound_dart/sound_dart.dart';

export 'package:sound_dart/sound_dart.dart';

/// Flutter-side conveniences over the `sound_dart` registry.
abstract final class SoundFlutter {
  /// Ensures a playback backend is selected and ready.
  ///
  /// On native platforms `sound_dart` already auto-registers the FFI backend that
  /// loads the bundled `sound_cli` library; this initializes it eagerly so
  /// the first [Sound.play] has no setup latency, and surfaces load errors
  /// early. Returns the name of the active backend (e.g. `ffi`, or `silent`
  /// when no audio device is available).
  static Future<String> ensureInitialized() async {
    return (await Sound.initialize()).name;
  }

  /// Loads a bundled Flutter asset (without starting it).
  ///
  /// [assetPath] is the key as declared in `pubspec.yaml` (e.g.
  /// `assets/chime.wav`). Pass [package] to load an asset that ships with
  /// another package, or [bundle] to read from a non-default [AssetBundle].
  static Future<Playback> loadAsset(
    String assetPath, {
    String? package,
    AssetBundle? bundle,
    double volume = 1.0,
    bool loop = false,
  }) async {
    final key = package == null ? assetPath : 'packages/$package/$assetPath';
    final data = await (bundle ?? rootBundle).load(key);
    final bytes = data.buffer.asUint8List(
      data.offsetInBytes,
      data.lengthInBytes,
    );
    return Sound.load(
      SoundSource.bytes(bytes, format: _extensionOf(assetPath)),
      volume: volume,
      loop: loop,
    );
  }

  /// Loads and immediately plays a bundled Flutter asset. See [loadAsset].
  static Future<Playback> playAsset(
    String assetPath, {
    String? package,
    AssetBundle? bundle,
    double volume = 1.0,
    bool loop = false,
  }) async {
    final playback = await loadAsset(
      assetPath,
      package: package,
      bundle: bundle,
      volume: volume,
      loop: loop,
    );
    await playback.play();
    return playback;
  }

  static String? _extensionOf(String path) {
    final dot = path.lastIndexOf('.');
    final slash = path.lastIndexOf('/');
    if (dot < 0 || dot < slash || dot == path.length - 1) return null;
    return path.substring(dot + 1).toLowerCase();
  }
}
