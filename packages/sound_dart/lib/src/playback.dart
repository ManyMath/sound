/// Lifecycle states of a [Playback].
enum PlaybackState {
  /// Loaded but not yet started.
  idle,

  /// A [SoundSource.uri] voice that has started but is still connecting,
  /// probing the stream's format, and decoding its first chunk - no audio
  /// has played yet. Backends without a connect phase (bytes/file sources,
  /// WebAudio) never emit this; they go straight to [playing].
  connecting,

  /// Currently producing audio.
  playing,

  /// Paused; can be resumed from the current position.
  paused,

  /// Stopped before reaching the end.
  stopped,

  /// Reached the end of the audio on its own.
  completed,

  /// Resources released; the handle can no longer be used.
  disposed,
}

/// A handle to a single loaded sound that can be controlled independently.
///
/// Backends return their own implementation from [SoundBackend.load]. Multiple
/// playbacks may exist at once so apps can layer sounds.
abstract class Playback {
  /// The current lifecycle state.
  PlaybackState get state;

  /// Whether audio is currently being produced.
  bool get isPlaying => state == PlaybackState.playing;

  /// Starts (or restarts) playback from the beginning.
  Future<void> play();

  /// Stops playback if it is running. Safe to call when already stopped.
  Future<void> stop();

  /// Sets the linear volume, where `1.0` is the original level. Values above
  /// `1.0` amplify (and may clip); `0.0` is silence.
  Future<void> setVolume(double volume);

  /// The last linear volume set (the `volume:` given to load until
  /// [setVolume] is called), with negative and NaN values clamped to `0.0`.
  double get volume;

  /// Enables or disables looping. Turning looping off lets the current pass
  /// finish and then completes naturally.
  Future<void> setLooping(bool looping);

  /// Pauses playback, keeping the current position. Safe to call when not
  /// playing.
  Future<void> pause();

  /// Resumes playback from the paused position. Safe to call when not paused.
  Future<void> resume();

  /// Seeks to [position] (clamped to the audio's length).
  Future<void> seek(Duration position);

  /// The current playback position.
  Duration get position;

  /// The total length of the audio, or `null` if not known.
  ///
  /// For a [SoundSource.uri] still downloading, this reflects how much has
  /// decoded *so far* and grows during playback until the stream finishes -
  /// the same way a browser's `<audio>`/`<video>` element reports a growing
  /// duration for streamed media before the full length is known.
  Duration? get duration;

  /// Completes when playback reaches the end on its own.
  ///
  /// Completes immediately if the sound has already finished, and never
  /// completes via this future if [stop] is called first (it resolves on
  /// natural completion only). Implementations may complete it on [dispose].
  Future<void> get onComplete;

  /// Releases native resources. The handle is unusable afterward.
  Future<void> dispose();
}
