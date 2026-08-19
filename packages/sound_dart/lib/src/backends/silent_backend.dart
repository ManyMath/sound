import 'dart:async';
import 'dart:typed_data';

import '../pcm_stream.dart';
import '../playback.dart';
import '../sound_backend.dart';
import '../sound_source.dart';

/// A backend that produces no audio.
///
/// It is the lowest-priority fallback so that `sound_dart` never crashes in
/// environments without an audio device (CI, headless servers). It is also a
/// convenient test double: every loaded source is recorded in [loaded]. As a
/// [PcmTap] it yields silence of [simulatedDuration], so the PCM-tap plumbing
/// can be exercised with no decoder or hardware.
class SilentBackend extends SoundBackend implements PcmTap {
  SilentBackend({
    this.simulatedDuration = Duration.zero,
    this.pcmSampleRate = 44100,
    this.pcmChannels = 1,
  });

  /// How long a [SilentPlayback] pretends to play before completing, and how
  /// much silence [openPcmStream] yields.
  final Duration simulatedDuration;

  /// Sample rate of the silence produced by [openPcmStream].
  final int pcmSampleRate;

  /// Channel count of the silence produced by [openPcmStream].
  final int pcmChannels;

  /// Sources passed to [load], in order. Useful in tests.
  final List<SoundSource> loaded = [];

  @override
  String get name => 'silent';

  @override
  bool get isAvailable => true;

  @override
  int get priority => -1000;

  @override
  Future<void> initialize() async {}

  @override
  Future<Playback> load(
    SoundSource source, {
    double volume = 1.0,
    bool loop = false,
  }) async {
    loaded.add(source);
    return SilentPlayback(simulatedDuration);
  }

  @override
  Future<PcmStream> openPcmStream(
    SoundSource source, {
    int chunkFrames = 2048,
    bool loop = false,
  }) async {
    loaded.add(source);
    final frames =
        simulatedDuration.inMicroseconds *
        pcmSampleRate ~/
        Duration.microsecondsPerSecond;
    return PcmStream(
      samples: Int16List(frames * pcmChannels), // all-zero silence
      channels: pcmChannels,
      sampleRate: pcmSampleRate,
      chunkFrames: chunkFrames,
      loop: loop,
    );
  }

  @override
  Future<void> dispose() async {}
}

/// The [Playback] returned by [SilentBackend].
class SilentPlayback implements Playback {
  SilentPlayback(this._duration);

  final Duration _duration;
  final Completer<void> _completer = Completer<void>();
  Timer? _timer;
  PlaybackState _state = PlaybackState.idle;

  @override
  PlaybackState get state => _state;

  @override
  bool get isPlaying => _state == PlaybackState.playing;

  @override
  Future<void> get onComplete => _completer.future;

  @override
  Future<void> play() async {
    if (_state == PlaybackState.disposed) return;
    _timer?.cancel();
    _state = PlaybackState.playing;
    if (_duration == Duration.zero) {
      _finish();
    } else {
      _timer = Timer(_duration, _finish);
    }
  }

  void _finish() {
    if (_state != PlaybackState.playing) return;
    _state = PlaybackState.completed;
    if (!_completer.isCompleted) _completer.complete();
  }

  @override
  Future<void> stop() async {
    if (_state == PlaybackState.disposed) return;
    _timer?.cancel();
    if (_state == PlaybackState.playing) _state = PlaybackState.stopped;
  }

  @override
  Future<void> setVolume(double volume) async {}

  @override
  Future<void> setLooping(bool looping) async {}

  @override
  Future<void> pause() async {
    if (_state == PlaybackState.playing) {
      _timer?.cancel();
      _state = PlaybackState.paused;
    }
  }

  @override
  Future<void> resume() async {
    if (_state == PlaybackState.paused) {
      _state = PlaybackState.playing;
      if (_duration == Duration.zero) {
        _finish();
      } else {
        _timer = Timer(_duration, _finish);
      }
    }
  }

  @override
  Future<void> seek(Duration position) async {}

  @override
  Duration get position => Duration.zero;

  @override
  Duration? get duration => _duration == Duration.zero ? null : _duration;

  @override
  Future<void> dispose() async {
    _timer?.cancel();
    _state = PlaybackState.disposed;
    if (!_completer.isCompleted) _completer.complete();
  }
}
