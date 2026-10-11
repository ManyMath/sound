import 'dart:async';
import 'dart:typed_data';

import 'sound_source.dart';

/// A backend capability for pulling decoded PCM without playing it.
///
/// A [PcmTap] decodes a [SoundSource] and exposes its interleaved S16LE frames
/// as a [PcmStream], reaching no audio device. This is the seam soundboard uses
/// to route a clip into a microphone capture stream instead of the speakers.
/// `sound_dart`'s FFI backend implements it with the native decoder; the silent
/// backend implements it with silence, so the tap works with no hardware.
abstract class PcmTap {
  /// Decodes [source] into a [PcmStream] of interleaved S16LE frames.
  ///
  /// [chunkFrames] is how many frames each emitted chunk holds; [loop] repeats
  /// the buffer instead of ending. Throws if the source cannot be decoded.
  Future<PcmStream> openPcmStream(
    SoundSource source, {
    int chunkFrames = 2048,
    bool loop = false,
  });
}

/// A decoded clip exposed both as a whole [buffer] and as a paced [frames]
/// stream of interleaved S16LE samples.
///
/// The samples are already decoded and held in memory. [buffer] hands back the
/// entire clip at once (used to cache a loaded clip), while [frames] paces it
/// out chunk by chunk at roughly real time once [start] is called -- the shape
/// a live tap into a capture pipeline wants. Listen to [frames] before calling
/// [start] so no early chunk is missed.
class PcmStream {
  PcmStream({
    required Int16List samples,
    required this.channels,
    required this.sampleRate,
    this.chunkFrames = 2048,
    this.loop = false,
  }) : assert(channels > 0, 'channels must be positive'),
       assert(sampleRate > 0, 'sampleRate must be positive'),
       assert(chunkFrames > 0, 'chunkFrames must be positive'),
       _samples = samples;

  final Int16List _samples;

  /// Channel count of the decoded audio (1 mono, 2 stereo, ...).
  final int channels;

  /// Sample rate of the decoded audio in Hz.
  final int sampleRate;

  /// Frames emitted per chunk on [frames].
  final int chunkFrames;

  /// Whether [frames] repeats the buffer instead of ending.
  final bool loop;

  // Broadcast so a closed-but-unlistened stream still completes cleanly (a
  // single-subscription controller's close() blocks until first listened).
  // Listen before calling [start] so no early chunk is missed.
  final StreamController<Int16List> _controller =
      StreamController<Int16List>.broadcast();
  final Completer<void> _onDone = Completer<void>();
  Timer? _timer;
  int _pos = 0; // interleaved sample index
  bool _started = false;
  bool _done = false;

  /// The complete decoded buffer (interleaved S16LE), useful to grab the whole
  /// clip at once instead of streaming it.
  Int16List get buffer => _samples;

  /// Total number of frames in the clip.
  int get frameCount => _samples.length ~/ channels;

  /// The clip's total length in time.
  Duration get duration => Duration(
    microseconds: frameCount * Duration.microsecondsPerSecond ~/ sampleRate,
  );

  /// Interleaved S16LE frames, paced at roughly real time once [start] is
  /// called. Closes when the audio is exhausted (unless [loop]) or on
  /// [stop] / [dispose].
  Stream<Int16List> get frames => _controller.stream;

  /// Completes when the stream finishes on its own (not via [stop]/[dispose]).
  ///
  /// [frames] closes either way; listen for its done event to observe both.
  Future<void> get onDone => _onDone.future;

  /// Begins emitting frames. Idempotent.
  Future<void> start() async {
    if (_started || _done) return;
    _started = true;
    final period = Duration(
      microseconds: chunkFrames * Duration.microsecondsPerSecond ~/ sampleRate,
    );
    _emit();
    if (!_done) _timer = Timer.periodic(period, (_) => _emit());
  }

  void _emit() {
    if (_done) return;
    if (_pos >= _samples.length) {
      if (loop && _samples.isNotEmpty) {
        _pos = 0;
      } else {
        _finish();
        return;
      }
    }
    final chunkSamples = chunkFrames * channels;
    final end = (_pos + chunkSamples) <= _samples.length
        ? _pos + chunkSamples
        : _samples.length;
    if (end > _pos) {
      if (!_controller.isClosed) {
        _controller.add(Int16List.sublistView(_samples, _pos, end));
      }
      _pos = end;
    }
    if (_pos >= _samples.length && !loop) _finish();
  }

  void _finish() {
    if (_done) return;
    _done = true;
    _timer?.cancel();
    _timer = null;
    if (!_controller.isClosed) _controller.close();
    if (!_onDone.isCompleted) _onDone.complete();
  }

  /// Stops emission and closes [frames] without completing it naturally.
  Future<void> stop() async {
    _done = true;
    _timer?.cancel();
    _timer = null;
    if (!_controller.isClosed) await _controller.close();
  }

  /// Releases the stream. The handle is unusable afterward.
  Future<void> dispose() => stop();
}
