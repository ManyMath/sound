import 'dart:async';

import 'playback.dart';

/// Volume-ramp helpers built on [Playback.setVolume], so they work with any
/// backend without backend-specific support.
extension SoundFade on Playback {
  bool get _ended =>
      state == PlaybackState.stopped ||
      state == PlaybackState.completed ||
      state == PlaybackState.disposed;

  /// Linearly ramps the volume from [from] to [to] over [duration] using
  /// [steps] increments. Stops early if the playback ends mid-ramp.
  ///
  /// [from] defaults to the current [volume].
  Future<void> fade(
    double? from,
    double to,
    Duration duration, {
    int steps = 20,
  }) async {
    from ??= volume;
    if (steps <= 0 || duration <= Duration.zero) {
      await setVolume(to);
      return;
    }
    await setVolume(from);
    final stepDelay = Duration(microseconds: duration.inMicroseconds ~/ steps);
    for (var i = 1; i <= steps; i++) {
      await Future<void>.delayed(stepDelay);
      if (_ended) return;
      await setVolume(from + (to - from) * (i / steps));
    }
  }

  /// Fades the volume up from `0.0` to [to] over [duration].
  Future<void> fadeIn(Duration duration, {double to = 1.0, int steps = 20}) =>
      fade(0, to, duration, steps: steps);

  /// Fades the volume down to `0.0` over [duration], then [stop]s by default.
  /// Starts from [from], or the current [volume] if omitted.
  Future<void> fadeOut(
    Duration duration, {
    double? from,
    int steps = 20,
    bool stop = true,
  }) async {
    await fade(from, 0, duration, steps: steps);
    if (stop) await this.stop();
  }
}
