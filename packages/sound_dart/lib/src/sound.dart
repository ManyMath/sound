import 'dart:typed_data';

import 'package:http/http.dart' as http;

import 'backends/native/native_backend.dart';
import 'backends/silent_backend.dart';
import 'exceptions.dart';
import 'pcm.dart';
import 'pcm_tap.dart' as pcm_tap;
import 'playback.dart';
import 'sound_backend.dart';
import 'sound_source.dart';

/// Entry point for playing audio.
///
/// `sound_dart` keeps a registry of [SoundBackend]s and picks the best available one
/// for the current environment (highest [SoundBackend.priority] among those
/// whose [SoundBackend.isAvailable] is true). Apps can register extra backends
/// or pin a specific one.
///
/// ```dart
/// final playback = await Sound.playFile('chime.wav');
/// await playback.onComplete;
/// ```
class Sound {
  Sound._();

  static final List<SoundBackend> _backends = [];
  static final Set<SoundBackend> _initialized = Set.identity();
  static SoundBackend? _active;
  static bool _defaultsRegistered = false;

  /// Registers the backends that ship with `sound_dart`.
  ///
  /// Backends from outside packages (e.g. sound_flutter) call
  /// [registerBackend] themselves; this only seeds the always-present floor.
  static void _ensureDefaults() {
    if (_defaultsRegistered) return;
    _defaultsRegistered = true;
    // Register the platform's native backend (FFI on native, none on web)
    // ahead of the silent fallback, which guarantees the API never crashes
    // for lack of a backend.
    final native = createNativeBackend();
    if (native != null) registerBackend(native);
    registerBackend(SilentBackend());
  }

  /// All registered backends, in registration order.
  static List<SoundBackend> get backends {
    _ensureDefaults();
    return List.unmodifiable(_backends);
  }

  /// Adds [backend] to the registry.
  ///
  /// Pass [makeActive] to select it immediately regardless of priority.
  /// Re-registering an instance is a no-op.
  static void registerBackend(SoundBackend backend, {bool makeActive = false}) {
    _ensureDefaults();
    if (!_backends.contains(backend)) _backends.add(backend);
    if (makeActive) {
      _active = backend;
    } else {
      // Force re-selection so a newly added, higher-priority backend wins.
      _active = null;
    }
  }

  /// The backend currently in use, selecting one on first access.
  ///
  /// Throws [NoBackendAvailableException] if nothing is available.
  static SoundBackend get backend {
    _ensureDefaults();
    return _active ??= _select();
  }

  /// Pins a specific registered backend by [name].
  ///
  /// Throws [NoBackendAvailableException] if no backend with that name is
  /// registered.
  static void useBackend(String name) {
    _ensureDefaults();
    final match = _backends.where((b) => b.name == name);
    if (match.isEmpty) {
      throw NoBackendAvailableException(
        'No backend named "$name" is registered.',
      );
    }
    _active = match.first;
  }

  static SoundBackend _select() {
    final available = _backends.where((b) => b.isAvailable).toList()
      ..sort((a, b) => b.priority.compareTo(a.priority));
    if (available.isEmpty) throw const NoBackendAvailableException();
    return available.first;
  }

  /// Loads [source] with the active backend, returning a controllable handle.
  ///
  /// [volume] sets the initial linear volume (`1.0` = original); [loop] repeats
  /// the audio until stopped.
  static Future<Playback> load(
    SoundSource source, {
    double volume = 1.0,
    bool loop = false,
  }) async {
    final b = backend;
    if (!_initialized.contains(b)) {
      await b.initialize();
      _initialized.add(b);
    }
    return b.load(source, volume: volume, loop: loop);
  }

  /// Loads and immediately starts [source].
  static Future<Playback> play(
    SoundSource source, {
    double volume = 1.0,
    bool loop = false,
  }) async {
    final playback = await load(source, volume: volume, loop: loop);
    await playback.play();
    return playback;
  }

  /// Convenience for [play] with a [FileSource].
  static Future<Playback> playFile(
    String path, {
    double volume = 1.0,
    bool loop = false,
  }) => play(SoundSource.file(path), volume: volume, loop: loop);

  /// Convenience for [play] with a [BytesSource].
  static Future<Playback> playBytes(
    Uint8List bytes, {
    String? format,
    double volume = 1.0,
    bool loop = false,
  }) => play(
    SoundSource.bytes(bytes, format: format),
    volume: volume,
    loop: loop,
  );

  /// Fetches [url] over HTTP and loads it (without starting). Works on every
  /// platform, including the web (where [FileSource] does not).
  static Future<Playback> loadUrl(
    String url, {
    double volume = 1.0,
    bool loop = false,
  }) async {
    final bytes = await _fetch(url);
    return load(
      SoundSource.bytes(bytes, format: _extensionOf(url)),
      volume: volume,
      loop: loop,
    );
  }

  /// Fetches [url] over HTTP and starts playing it.
  static Future<Playback> playUrl(
    String url, {
    double volume = 1.0,
    bool loop = false,
  }) async {
    final playback = await loadUrl(url, volume: volume, loop: loop);
    await playback.play();
    return playback;
  }

  static Future<Uint8List> _fetch(String url) async {
    final Uri uri;
    try {
      uri = Uri.parse(url);
    } on FormatException catch (e) {
      throw SoundException('invalid URL "$url": ${e.message}');
    }
    final response = await http.get(uri);
    if (response.statusCode != 200) {
      throw SoundException('failed to fetch $url: HTTP ${response.statusCode}');
    }
    return response.bodyBytes;
  }

  /// The lowercase file extension of a URL/path, or `null` if there is none.
  static String? _extensionOf(String url) {
    final path = Uri.tryParse(url)?.path ?? url;
    final dot = path.lastIndexOf('.');
    final slash = path.lastIndexOf('/');
    if (dot < 0 || dot < slash || dot == path.length - 1) return null;
    return path.substring(dot + 1).toLowerCase();
  }

  /// Opens [source] as a [PcmStream] of decoded frames without playing it.
  ///
  /// This is the playback-path "tap": the clip is pulled as data (to mix,
  /// analyze, or re-route) rather than sent to an audio device, so it works
  /// under any backend, including [SilentBackend]. 16-bit PCM WAV is decoded in
  /// pure Dart; compressed formats are a roadmap item. [chunkFrames] sets how
  /// many frames each emitted Int16List carries. Throws
  /// [UnsupportedSourceException] for a source the tap cannot decode.
  static Future<PcmStream> openPcmStream(
    SoundSource source, {
    int chunkFrames = 2048,
  }) => pcm_tap.openPcmStream(source, chunkFrames: chunkFrames);

  /// Disposes the active backend and clears selection/registry.
  ///
  /// Primarily for tests; after this the defaults are re-seeded on next use.
  static Future<void> reset() async {
    for (final b in _initialized) {
      await b.dispose();
    }
    _initialized.clear();
    _backends.clear();
    _active = null;
    _defaultsRegistered = false;
  }
}
