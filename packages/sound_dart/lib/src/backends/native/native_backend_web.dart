import 'dart:async';
import 'dart:js_interop';
import 'dart:typed_data';

import 'package:http/http.dart' as http;
import 'package:web/web.dart' as web;

import '../../exceptions.dart';
import '../../playback.dart';
import '../../sound_backend.dart';
import '../../sound_source.dart';

/// On the web the default backend uses the WebAudio API.
SoundBackend? createNativeBackend() => WebAudioBackend();

/// Plays audio in the browser via WebAudio.
///
/// The browser decodes the container itself (WAV/MP3/OGG/…), so [BytesSource]
/// works for any format the browser supports. [FileSource] is not available on
/// the web - load file contents and pass them as bytes instead. [UriSource]
/// fetches the whole response (with headers, e.g. an auth token) before
/// decoding - `decodeAudioData` has no incremental/streaming API, so unlike
/// the native FFI backend's `sound_play_url`, this does not start playback
/// before the download completes.
class WebAudioBackend extends SoundBackend {
  web.AudioContext? _context;

  @override
  String get name => 'webaudio';

  @override
  int get priority => 100;

  @override
  bool get isAvailable => true;

  @override
  Future<void> initialize() async {
    _context ??= web.AudioContext();
  }

  @override
  Future<Playback> load(
    SoundSource source, {
    double volume = 1.0,
    bool loop = false,
  }) async {
    await initialize();
    final bytes = switch (source) {
      BytesSource(:final bytes) => bytes,
      FileSource() => throw const UnsupportedSourceException(
        'FileSource is not supported on the web; pass bytes instead.',
      ),
      UriSource(:final uri, :final headers) => await _fetch(uri, headers),
    };
    // decodeAudioData detaches its input, so hand it a private copy.
    final copy = Uint8List.fromList(bytes);
    final web.AudioBuffer buffer;
    try {
      buffer = await _context!.decodeAudioData(copy.buffer.toJS).toDart;
    } on Object catch (e) {
      // The browser rejects with a DOMException (EncodingError).
      throw UnsupportedSourceException('could not decode audio: $e');
    }
    return WebAudioPlayback(_context!, buffer, volume, loop);
  }

  @override
  Future<void> dispose() async {
    await _context?.close().toDart;
    _context = null;
  }

  /// Fetches `uri` with `headers` (e.g. an auth token), throwing a
  /// [PlaybackException] on a network failure or non-2xx response.
  Future<Uint8List> _fetch(Uri uri, Map<String, String> headers) async {
    final http.Response response;
    try {
      response = await http.get(uri, headers: headers);
    } on http.ClientException catch (e) {
      throw PlaybackException('fetching $uri failed: ${e.message}');
    }
    if (response.statusCode < 200 || response.statusCode >= 300) {
      throw PlaybackException(
        'fetching $uri failed: HTTP ${response.statusCode}',
      );
    }
    return response.bodyBytes;
  }
}

/// A single WebAudio voice backed by a decoded [web.AudioBuffer].
class WebAudioPlayback implements Playback {
  WebAudioPlayback(this._context, this._buffer, double volume, this._looping)
    : _gain = _context.createGain() {
    _volume = _gainFor(volume);
    _gain.gain.value = _volume;
    _gain.connect(_context.destination);
  }

  // A negative gain would invert the waveform rather than silence it, and
  // the AudioParam rejects NaN; both clamp to 0 like the native backend.
  static double _gainFor(double volume) => volume > 0 ? volume : 0.0;

  final web.AudioContext _context;
  final web.AudioBuffer _buffer;
  final web.GainNode _gain;
  late double _volume;
  bool _looping;

  web.AudioBufferSourceNode? _source;
  PlaybackState _state = PlaybackState.idle;
  // Replaced only once used up, so a future taken before play() still
  // completes.
  Completer<void> _completer = Completer<void>();

  // Offset into the buffer (seconds) the current source started from, and the
  // AudioContext time when it started - together they give the position.
  double _offset = 0;
  double _startedAt = 0;

  @override
  PlaybackState get state => _state;

  @override
  bool get isPlaying => _state == PlaybackState.playing;

  @override
  Future<void> get onComplete => _completer.future;

  @override
  Future<void> play() async {
    if (_state == PlaybackState.disposed) return;
    if (_completer.isCompleted) _completer = Completer<void>();
    _startSource(0);
    _state = PlaybackState.playing;
  }

  /// (Re)starts a buffer source playing from [offset] seconds.
  void _startSource(double offset) {
    _stopSource();
    // A context created before any user gesture starts suspended and stays
    // so until resumed. Not awaited: the browser may hold the promise until
    // a gesture, and a source started meanwhile plays once it runs.
    if (_context.state == 'suspended') _context.resume().toDart.ignore();
    _offset = offset.clamp(0, _buffer.duration);
    _startedAt = _context.currentTime.toDouble();
    final source = web.AudioBufferSourceNode(_context)
      ..buffer = _buffer
      ..loop = _looping;
    source.connect(_gain);
    source.onended = (web.Event _) {
      if (_state == PlaybackState.playing) {
        _state = PlaybackState.completed;
        if (!_completer.isCompleted) _completer.complete();
      }
    }.toJS;
    source.start(0, _offset);
    _source = source;
  }

  void _stopSource() {
    final source = _source;
    if (source != null) {
      source.onended = null;
      try {
        source.stop();
      } on Object {
        // Already stopped / never started.
      }
      _source = null;
    }
  }

  @override
  Future<void> stop() async {
    _stopSource();
    if (_state == PlaybackState.playing || _state == PlaybackState.paused) {
      _state = PlaybackState.stopped;
    }
  }

  @override
  Future<void> setVolume(double volume) async {
    _volume = _gainFor(volume);
    _gain.gain.value = _volume;
  }

  @override
  double get volume => _volume;

  @override
  Future<void> setLooping(bool looping) async {
    if (_state == PlaybackState.playing && looping != _looping) {
      // Re-anchor at the current spot so position stays right after the
      // wrap-around in [position] switches on or off.
      _offset = position.inMicroseconds / 1e6;
      _startedAt = _context.currentTime.toDouble();
    }
    _looping = looping;
    _source?.loop = looping;
  }

  @override
  Future<void> pause() async {
    if (_state != PlaybackState.playing) return;
    final at = position; // capture before stopping
    _stopSource();
    _offset = at.inMicroseconds / 1e6;
    _state = PlaybackState.paused;
  }

  @override
  Future<void> resume() async {
    if (_state != PlaybackState.paused) return;
    _startSource(_offset);
    _state = PlaybackState.playing;
  }

  @override
  Future<void> seek(Duration position) async {
    final offset = (position.inMicroseconds / 1e6).clamp(0, _buffer.duration);
    if (_state == PlaybackState.playing) {
      _startSource(offset.toDouble());
    } else {
      _offset = offset.toDouble();
    }
  }

  @override
  Duration get position {
    final length = _buffer.duration;
    var seconds = _state == PlaybackState.playing
        ? _offset + (_context.currentTime.toDouble() - _startedAt)
        : _offset;
    // A looping source wraps back to the start each time it reaches the end.
    if (_looping && length > 0) seconds %= length;
    final clamped = seconds.clamp(0, length);
    return Duration(microseconds: (clamped * 1e6).round());
  }

  @override
  Duration? get duration =>
      Duration(microseconds: (_buffer.duration * 1e6).round());

  @override
  Future<void> dispose() async {
    _stopSource();
    _state = PlaybackState.disposed;
    if (!_completer.isCompleted) _completer.complete();
  }
}
