import 'dart:typed_data';

import 'backends/native/native_backend.dart';
import 'backends/silent_backend.dart';
import 'exceptions.dart';
import 'pcm_stream.dart';
import 'playback.dart';
import 'sound_backend.dart';
import 'sound_source.dart';

/// Entry point for playing audio.
///
/// `sound_dart` keeps a registry of [SoundBackend]s and picks the best available one
/// for the current environment (highest [SoundBackend.priority] among those
/// whose [SoundBackend.isAvailable] is true). Apps can register extra backends
/// or pin a specific one. If an automatically selected backend fails to
/// initialize, it is skipped and the next best one is used; a pinned
/// backend's failure is rethrown instead.
///
/// ```dart
/// final playback = await Sound.playFile('chime.wav');
/// await playback.onComplete;
/// await playback.dispose();
/// ```
class Sound {
  Sound._();

  static final List<SoundBackend> _backends = [];
  static final Set<SoundBackend> _initialized = Set.identity();
  // Auto-selected backends whose initialize() threw; selection skips them.
  static final Set<SoundBackend> _failed = Set.identity();
  static SoundBackend? _active;
  // Set when _active was chosen explicitly rather than by priority.
  static bool _pinned = false;
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
  /// Pass [makeActive] to select it immediately regardless of priority; a
  /// backend selected that way (or with [useBackend]) stays active when
  /// others are registered later. Re-registering an instance is a no-op.
  static void registerBackend(SoundBackend backend, {bool makeActive = false}) {
    _ensureDefaults();
    final added = !_backends.contains(backend);
    if (added) _backends.add(backend);
    if (makeActive) {
      _active = backend;
      _pinned = true;
    } else if (added && !_pinned) {
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
    _pinned = true;
  }

  static SoundBackend _select() {
    final available =
        _backends.where((b) => b.isAvailable && !_failed.contains(b)).toList()
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
    final b = await _ready();
    return b.load(source, volume: volume, loop: loop);
  }

  /// Initializes and returns the active backend without loading anything.
  ///
  /// An auto-selected backend whose `initialize()` throws is skipped in favor
  /// of the next one; a pinned backend (see [useBackend]) rethrows instead.
  /// [load] does the same implicitly.
  static Future<SoundBackend> initialize() => _ready();

  /// The active backend, initialized, falling back past auto-selected
  /// backends whose initialize() throws.
  static Future<SoundBackend> _ready() async {
    while (true) {
      final b = backend;
      if (_initialized.contains(b)) return b;
      try {
        await b.initialize();
        _initialized.add(b);
        return b;
      } on Exception {
        if (_pinned && identical(_active, b)) rethrow;
        _failed.add(b);
        if (identical(_active, b)) _active = null;
      }
    }
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

  /// Loads [url] as a [SoundSource.uri] (without starting), sending [headers]
  /// with the request (e.g. an `Authorization` bearer token). Works on every
  /// platform, including the web (where [FileSource] does not). On a native
  /// backend that supports it, playback can start before [url] finishes
  /// downloading; see [SoundSource.uri].
  ///
  /// [headers] are only sent to [url]'s own origin; a redirect to another
  /// host, port, or scheme drops them so credentials cannot leak. Note the
  /// native backend keeps both the download and the decoded PCM in memory for
  /// the playback's lifetime (which makes seek/loop instant), so this suits
  /// tracks, not open-ended live streams.
  static Future<Playback> loadUrl(
    String url, {
    Map<String, String> headers = const {},
    double volume = 1.0,
    bool loop = false,
  }) async {
    final Uri uri;
    try {
      uri = Uri.parse(url);
    } on FormatException catch (e) {
      throw SoundException('invalid URL "$url": ${e.message}');
    }
    return load(
      SoundSource.uri(uri, headers: headers, format: _extensionOf(url)),
      volume: volume,
      loop: loop,
    );
  }

  /// Loads [url] (with [headers]) and starts playing it.
  static Future<Playback> playUrl(
    String url, {
    Map<String, String> headers = const {},
    double volume = 1.0,
    bool loop = false,
  }) async {
    final playback = await loadUrl(
      url,
      headers: headers,
      volume: volume,
      loop: loop,
    );
    await playback.play();
    return playback;
  }

  /// The lowercase file extension of a URL/path, or `null` if there is none.
  static String? _extensionOf(String url) {
    final path = Uri.tryParse(url)?.path ?? url;
    final dot = path.lastIndexOf('.');
    final slash = path.lastIndexOf('/');
    if (dot < 0 || dot < slash || dot == path.length - 1) return null;
    return path.substring(dot + 1).toLowerCase();
  }

  /// Opens a [PcmStream] of [source]'s decoded interleaved S16LE frames without
  /// playing it through an audio device.
  ///
  /// This routes a clip's samples somewhere other than the speakers -- for
  /// example into a microphone capture stream. It uses the active backend when
  /// it is a [PcmTap] (the FFI backend on native), otherwise the
  /// highest-priority registered backend that supports tapping (the silent
  /// fallback always does). [chunkFrames] sizes each emitted chunk; [loop]
  /// repeats the buffer.
  static Future<PcmStream> openPcmStream(
    SoundSource source, {
    int chunkFrames = 2048,
    bool loop = false,
  }) {
    final tap = _selectTap();
    return tap.openPcmStream(source, chunkFrames: chunkFrames, loop: loop);
  }

  /// Picks a backend that can tap PCM: the active one if it can, else the
  /// highest-priority available [PcmTap].
  static PcmTap _selectTap() {
    _ensureDefaults();
    final active = backend;
    if (active is PcmTap) return active as PcmTap;
    final taps = _backends.where((b) => b.isAvailable && b is PcmTap).toList()
      ..sort((a, b) => b.priority.compareTo(a.priority));
    if (taps.isEmpty) {
      throw const UnsupportedSourceException(
        'No registered backend supports a PCM tap.',
      );
    }
    return taps.first as PcmTap;
  }

  /// Disposes the active backend and clears selection/registry.
  ///
  /// Primarily for tests; after this the defaults are re-seeded on next use.
  static Future<void> reset() async {
    for (final b in _initialized) {
      await b.dispose();
    }
    _initialized.clear();
    _failed.clear();
    _backends.clear();
    _active = null;
    _pinned = false;
    _defaultsRegistered = false;
  }
}
