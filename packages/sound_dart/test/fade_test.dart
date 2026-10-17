import 'package:sound_dart/sound_dart.dart';
import 'package:test/test.dart';

/// A [Playback] that records every volume set, for testing fade ramps without
/// audio.
class RecordingPlayback implements Playback {
  final List<double> volumes = [];
  @override
  double volume = 1.0;
  @override
  PlaybackState state = PlaybackState.playing;
  bool stopped = false;

  @override
  Future<void> setVolume(double volume) async {
    this.volume = volume;
    volumes.add(volume);
  }

  @override
  Future<void> stop() async {
    stopped = true;
    state = PlaybackState.stopped;
  }

  // Unused by fade tests.
  @override
  bool get isPlaying => state == PlaybackState.playing;
  @override
  Duration get position => Duration.zero;
  @override
  Duration? get duration => null;
  @override
  Future<void> get onComplete async {}
  @override
  Future<void> play() async {}
  @override
  Future<void> pause() async {}
  @override
  Future<void> resume() async {}
  @override
  Future<void> seek(Duration position) async {}
  @override
  Future<void> setLooping(bool looping) async {}
  @override
  Future<void> dispose() async => state = PlaybackState.disposed;
}

bool isMonotonicIncreasing(List<double> xs) {
  for (var i = 1; i < xs.length; i++) {
    if (xs[i] < xs[i - 1] - 1e-9) return false;
  }
  return true;
}

void main() {
  test('fadeIn ramps from 0 to the target, monotonically', () async {
    final pb = RecordingPlayback();
    await pb.fadeIn(const Duration(milliseconds: 50), steps: 5);
    expect(pb.volumes.first, 0);
    expect(pb.volumes.last, closeTo(1.0, 1e-9));
    expect(isMonotonicIncreasing(pb.volumes), isTrue);
  });

  test('fadeOut ramps to 0 and stops by default', () async {
    final pb = RecordingPlayback();
    await pb.fadeOut(const Duration(milliseconds: 50), steps: 5);
    expect(pb.volumes.first, 1.0); // default volume
    expect(pb.volumes.last, closeTo(0.0, 1e-9));
    expect(isMonotonicIncreasing(pb.volumes.reversed.toList()), isTrue);
    expect(pb.stopped, isTrue);
  });

  test('fadeOut starts from the current volume', () async {
    final pb = RecordingPlayback()..volume = 0.2;
    await pb.fadeOut(const Duration(milliseconds: 50), steps: 5);
    expect(pb.volumes.first, 0.2);
    expect(pb.volumes.every((v) => v <= 0.2), isTrue);
    expect(pb.volumes.last, closeTo(0.0, 1e-9));
  });

  test('fade with no from starts at the current volume', () async {
    final pb = RecordingPlayback()..volume = 0.4;
    await pb.fade(null, 1, const Duration(milliseconds: 50), steps: 5);
    expect(pb.volumes.first, 0.4);
    expect(pb.volumes.last, closeTo(1.0, 1e-9));
  });

  test('fadeOut(from:) overrides the current volume', () async {
    final pb = RecordingPlayback()..volume = 0.2;
    await pb.fadeOut(const Duration(milliseconds: 50), from: 0.8, steps: 5);
    expect(pb.volumes.first, 0.8);
  });

  test('fadeOut(stop: false) does not stop', () async {
    final pb = RecordingPlayback();
    await pb.fadeOut(const Duration(milliseconds: 20), steps: 4, stop: false);
    expect(pb.stopped, isFalse);
  });

  test('zero duration jumps straight to the target', () async {
    final pb = RecordingPlayback();
    await pb.fade(0, 1, Duration.zero);
    expect(pb.volumes, [1.0]);
  });

  test('fade stops early when the playback ends', () async {
    final pb = RecordingPlayback();
    final future = pb.fade(0, 1, const Duration(milliseconds: 200), steps: 20);
    pb.state = PlaybackState.completed; // ends mid-ramp
    await future;
    // It should not have driven the ramp all the way to 1.0.
    expect(pb.volumes.last, lessThan(1.0));
  });
}
