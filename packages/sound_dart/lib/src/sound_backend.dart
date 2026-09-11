import 'playback.dart';
import 'sound_source.dart';

/// A pluggable provider of sound capabilities for a given platform/technique.
///
/// `sound_dart` is backend-agnostic: Linux/macOS/Windows/CLI reach
/// native audio over FFI, Web uses WebAudio, and Flutter platforms may layer
/// channel-based backends. Several backends can be registered at once; the
/// active one is chosen by [isAvailable] and [priority] (see `Sound`).
abstract class SoundBackend {
  /// A short, stable identifier (e.g. `ffi`, `webaudio`, `silent`).
  String get name;

  /// Whether this backend can run in the current environment.
  ///
  /// Should be cheap and side-effect free; it is consulted during backend
  /// selection. For example the FFI backend reports `false` when the native
  /// library cannot be located/loaded.
  bool get isAvailable;

  /// Selection weight when multiple backends are available; higher wins.
  int get priority;

  /// Prepares the backend for use. Called once before the first [load].
  ///
  /// Must be idempotent - selection may initialize a backend that was already
  /// initialized.
  Future<void> initialize();

  /// Loads [source] and returns a controllable [Playback].
  ///
  /// [volume] sets the initial linear volume (`1.0` = original); [loop] repeats
  /// the audio until stopped. Throws [UnsupportedSourceException] if the source
  /// kind/format is not supported by this backend.
  Future<Playback> load(
    SoundSource source, {
    double volume = 1.0,
    bool loop = false,
  });

  /// Releases any global resources held by the backend.
  Future<void> dispose();
}
