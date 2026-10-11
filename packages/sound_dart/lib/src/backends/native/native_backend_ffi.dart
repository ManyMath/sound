import 'dart:async';
import 'dart:ffi';
import 'dart:io';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import '../../exceptions.dart';
import '../../pcm_stream.dart';
import '../../playback.dart';
import '../../sound_backend.dart';
import '../../sound_source.dart';

/// The default native backend on platforms with `dart:ffi`.
SoundBackend? createNativeBackend() => FfiBackend();

// --- C ABI signatures, mirroring packages/sound_flutter/rust/src/lib.rs ---

typedef _PlayerNewNative = Pointer<Void> Function();
typedef _PlayerFreeNative = Void Function(Pointer<Void>);
typedef _PlayerFree = void Function(Pointer<Void>);
typedef _PlayBytesNative =
    Uint64 Function(Pointer<Void>, Pointer<Uint8>, Size, Pointer<Utf8>, Int32);
typedef _PlayBytes =
    int Function(Pointer<Void>, Pointer<Uint8>, int, Pointer<Utf8>, int);
typedef _PlayFileNative = Uint64 Function(Pointer<Void>, Pointer<Utf8>, Int32);
typedef _PlayFile = int Function(Pointer<Void>, Pointer<Utf8>, int);
typedef _PlayUrlNative =
    Uint64 Function(
      Pointer<Void>,
      Pointer<Utf8>,
      Pointer<Pointer<Utf8>>,
      Pointer<Pointer<Utf8>>,
      Size,
      Pointer<Utf8>,
      Int32,
    );
typedef _PlayUrl =
    int Function(
      Pointer<Void>,
      Pointer<Utf8>,
      Pointer<Pointer<Utf8>>,
      Pointer<Pointer<Utf8>>,
      int,
      Pointer<Utf8>,
      int,
    );
typedef _VoiceStateNative = Int32 Function(Pointer<Void>, Uint64);
typedef _VoiceState = int Function(Pointer<Void>, int);
typedef _VoiceOpNative = Int32 Function(Pointer<Void>, Uint64);
typedef _VoiceOp = int Function(Pointer<Void>, int);
typedef _SetVolumeNative = Int32 Function(Pointer<Void>, Uint64, Float);
typedef _SetVolume = int Function(Pointer<Void>, int, double);
typedef _SetLoopNative = Int32 Function(Pointer<Void>, Uint64, Int32);
typedef _SetLoop = int Function(Pointer<Void>, int, int);
typedef _SeekNative = Int32 Function(Pointer<Void>, Uint64, Uint64);
typedef _Seek = int Function(Pointer<Void>, int, int);
typedef _I64QueryNative = Int64 Function(Pointer<Void>, Uint64);
typedef _I64Query = int Function(Pointer<Void>, int);
typedef _VoiceErrorNative = Pointer<Utf8> Function(Pointer<Void>, Uint64);
typedef _VoiceError = Pointer<Utf8> Function(Pointer<Void>, int);
typedef _LastErrorNative = Pointer<Utf8> Function();
typedef _LastError = Pointer<Utf8> Function();
// Decode-to-PCM (the PCM tap), mirroring
// packages/sound_flutter/rust/src/lib.rs.
typedef _DecodeBytesNative =
    Pointer<Void> Function(Pointer<Uint8>, Size, Pointer<Utf8>);
typedef _DecodeBytes =
    Pointer<Void> Function(Pointer<Uint8>, int, Pointer<Utf8>);
typedef _DecodeFileNative = Pointer<Void> Function(Pointer<Utf8>);
typedef _DecodeFile = Pointer<Void> Function(Pointer<Utf8>);
typedef _DecodedLenNative = Int64 Function(Pointer<Void>);
typedef _DecodedLen = int Function(Pointer<Void>);
typedef _DecodedIntNative = Int32 Function(Pointer<Void>);
typedef _DecodedInt = int Function(Pointer<Void>);
typedef _DecodedCopyNative =
    IntPtr Function(Pointer<Void>, Size, Pointer<Int16>, Size);
typedef _DecodedCopy = int Function(Pointer<Void>, int, Pointer<Int16>, int);
typedef _DecodedFreeNative = Void Function(Pointer<Void>);
typedef _DecodedFree = void Function(Pointer<Void>);

// Voice state codes mirrored from the native side.
const int _statePlaying = 0;
const int _stateCompleted = 1;
const int _stateStopped = 2;
const int _stateError = 3;
const int _statePaused = 4;
const int _stateConnecting = 5;

/// Locates and opens the `sound_cli` shared library.
class NativeLibrary {
  /// Explicit path override, highest priority. Set before first use.
  static String? overridePath;

  static DynamicLibrary? _opened;
  static bool _attempted = false;

  static String get _fileName => switch (Platform.operatingSystem) {
    'windows' => 'sound_cli.dll',
    'macos' || 'ios' => 'libsound_cli.dylib',
    _ => 'libsound_cli.so',
  };

  /// Opens the library, caching the result. Returns `null` if it cannot be
  /// found/loaded - never throws.
  static DynamicLibrary? open() {
    if (_attempted) return _opened;
    _attempted = true;
    for (final candidate in _candidates()) {
      try {
        _opened = DynamicLibrary.open(candidate);
        return _opened;
      } on Object {
        // Try the next candidate.
      }
    }
    // On Apple platforms the symbols are -force_load'd into the plugin's
    // framework, which is loaded into the host process for us - so process()
    // resolves them even when DynamicLibrary.open cannot find the framework
    // path directly.
    if (Platform.isMacOS || Platform.isIOS) {
      try {
        final p = DynamicLibrary.process();
        // Probe one expected symbol so we know FFI can find it.
        p.lookup<NativeFunction<_PlayerNewNative>>('sound_player_new');
        _opened = p;
      } on Object {
        // Fall through.
      }
    }
    return _opened;
  }

  static Iterable<String> _candidates() sync* {
    if (overridePath != null) yield overridePath!;
    final fromEnv = Platform.environment['SOUND_DART_LIB'];
    if (fromEnv != null && fromEnv.isNotEmpty) yield fromEnv;
    // Resolved via the OS loader path / Flutter app bundle.
    yield _fileName;
    // On macOS/iOS the symbols are -force_load'd into the plugin's framework,
    // so the lib name lookup above fails. The Flutter loader resolves the
    // framework binary by its short path.
    if (Platform.isMacOS || Platform.isIOS) {
      yield 'sound_flutter.framework/sound_flutter';
    }
    // Developer builds of the in-repo crate, searched from the cwd upward.
    var dir = Directory.current.absolute;
    for (var i = 0; i < 6; i++) {
      for (final profile in ['release', 'debug']) {
        yield '${dir.path}/packages/sound_flutter/rust/target/$profile/$_fileName';
      }
      final parent = dir.parent;
      if (parent.path == dir.path) break;
      dir = parent;
    }
  }
}

/// Bound entry points into the native library.
class _Bindings {
  _Bindings(DynamicLibrary lib)
    : playerNew = lib.lookupFunction<_PlayerNewNative, _PlayerNewNative>(
        'sound_player_new',
      ),
      playerFree = lib.lookupFunction<_PlayerFreeNative, _PlayerFree>(
        'sound_player_free',
      ),
      playBytes = lib.lookupFunction<_PlayBytesNative, _PlayBytes>(
        'sound_play_bytes',
      ),
      playFile = lib.lookupFunction<_PlayFileNative, _PlayFile>(
        'sound_play_file',
      ),
      playUrl = lib.lookupFunction<_PlayUrlNative, _PlayUrl>('sound_play_url'),
      voiceState = lib.lookupFunction<_VoiceStateNative, _VoiceState>(
        'sound_voice_state',
      ),
      stop = lib.lookupFunction<_VoiceOpNative, _VoiceOp>('sound_stop'),
      setVolume = lib.lookupFunction<_SetVolumeNative, _SetVolume>(
        'sound_set_volume',
      ),
      setLoop = lib.lookupFunction<_SetLoopNative, _SetLoop>('sound_set_loop'),
      pause = lib.lookupFunction<_VoiceOpNative, _VoiceOp>('sound_pause'),
      resume = lib.lookupFunction<_VoiceOpNative, _VoiceOp>('sound_resume'),
      seek = lib.lookupFunction<_SeekNative, _Seek>('sound_seek'),
      position = lib.lookupFunction<_I64QueryNative, _I64Query>(
        'sound_position',
      ),
      durationFrames = lib.lookupFunction<_I64QueryNative, _I64Query>(
        'sound_duration_frames',
      ),
      sampleRate = lib.lookupFunction<_VoiceStateNative, _VoiceState>(
        'sound_sample_rate',
      ),
      voiceFree = lib.lookupFunction<_VoiceOpNative, _VoiceOp>(
        'sound_voice_free',
      ),
      voiceError = lib.lookupFunction<_VoiceErrorNative, _VoiceError>(
        'sound_voice_error',
      ),
      lastError = lib.lookupFunction<_LastErrorNative, _LastError>(
        'sound_last_error',
      ),
      decodeBytes = lib.lookupFunction<_DecodeBytesNative, _DecodeBytes>(
        'sound_decode_bytes',
      ),
      decodeFile = lib.lookupFunction<_DecodeFileNative, _DecodeFile>(
        'sound_decode_file',
      ),
      decodedLen = lib.lookupFunction<_DecodedLenNative, _DecodedLen>(
        'sound_decoded_len',
      ),
      decodedChannels = lib.lookupFunction<_DecodedIntNative, _DecodedInt>(
        'sound_decoded_channels',
      ),
      decodedRate = lib.lookupFunction<_DecodedIntNative, _DecodedInt>(
        'sound_decoded_rate',
      ),
      decodedCopy = lib.lookupFunction<_DecodedCopyNative, _DecodedCopy>(
        'sound_decoded_copy',
      ),
      decodedFree = lib.lookupFunction<_DecodedFreeNative, _DecodedFree>(
        'sound_decoded_free',
      );

  final Pointer<Void> Function() playerNew;
  final _PlayerFree playerFree;
  final _PlayBytes playBytes;
  final _PlayFile playFile;
  final _PlayUrl playUrl;
  final _VoiceState voiceState;
  final _VoiceOp stop;
  final _SetVolume setVolume;
  final _SetLoop setLoop;
  final _VoiceOp pause;
  final _VoiceOp resume;
  final _Seek seek;
  final _I64Query position;
  final _I64Query durationFrames;
  final _VoiceState sampleRate;
  final _VoiceOp voiceFree;
  final _VoiceError voiceError;
  final _LastError lastError;
  final _DecodeBytes decodeBytes;
  final _DecodeFile decodeFile;
  final _DecodedLen decodedLen;
  final _DecodedInt decodedChannels;
  final _DecodedInt decodedRate;
  final _DecodedCopy decodedCopy;
  final _DecodedFree decodedFree;
}

/// Plays audio by calling the native `sound_cli` library over FFI.
///
/// It also implements [PcmTap]: the native decoder can hand back decoded PCM
/// without opening an audio device, which is how a clip is routed into a
/// capture stream instead of the speakers.
class FfiBackend extends SoundBackend implements PcmTap {
  _Bindings? _bindings;
  Pointer<Void> _player = nullptr;
  // Bumped whenever the player is freed, so playbacks can tell that voice
  // ids they hold now belong to a different (or no) player.
  int _generation = 0;
  bool _available = false;
  bool _probed = false;

  @override
  String get name => 'ffi';

  @override
  int get priority => 100;

  @override
  bool get isAvailable {
    if (_probed) return _available;
    _probed = true;
    final lib = NativeLibrary.open();
    if (lib == null) return _available = false;
    try {
      _bindings = _Bindings(lib);
      _available = true;
    } on Object {
      _available = false;
    }
    return _available;
  }

  @override
  Future<void> initialize() async {
    if (!isAvailable) {
      throw const NoBackendAvailableException(
        'The sound_cli library could not be loaded.',
      );
    }
    if (_player != nullptr) return;
    _player = _bindings!.playerNew();
    if (_player == nullptr) {
      throw PlaybackException('sound_player_new failed: ${_lastError()}');
    }
  }

  String _lastError() {
    final ptr = _bindings!.lastError();
    return ptr == nullptr ? 'unknown error' : ptr.toDartString();
  }

  @override
  Future<Playback> load(
    SoundSource source, {
    double volume = 1.0,
    bool loop = false,
  }) async {
    if (_player == nullptr) await initialize();
    return FfiPlayback._(this, source, volume, loop);
  }

  @override
  Future<PcmStream> openPcmStream(
    SoundSource source, {
    int chunkFrames = 2048,
    bool loop = false,
  }) async {
    if (!isAvailable) {
      throw const NoBackendAvailableException(
        'The sound_cli library could not be loaded.',
      );
    }
    return _decode(source, chunkFrames: chunkFrames, loop: loop);
  }

  /// Decodes [source] to interleaved S16LE via the native decoder (no audio
  /// device) and copies the samples into a Dart-owned [PcmStream].
  PcmStream _decode(
    SoundSource source, {
    required int chunkFrames,
    required bool loop,
  }) {
    final b = _bindings!;
    final Pointer<Void> decoded;
    switch (source) {
      case BytesSource(:final bytes, :final format):
        final buf = malloc<Uint8>(bytes.length);
        final cFormat = format == null ? nullptr : format.toNativeUtf8();
        try {
          buf.asTypedList(bytes.length).setAll(0, bytes);
          decoded = b.decodeBytes(buf, bytes.length, cFormat);
        } finally {
          malloc.free(buf);
          if (cFormat != nullptr) malloc.free(cFormat);
        }
      case FileSource(:final path):
        final cPath = path.toNativeUtf8();
        try {
          decoded = b.decodeFile(cPath);
        } finally {
          malloc.free(cPath);
        }
      case UriSource():
        // The no-device decode-to-PCM tap (used by `soundboard`'s mic
        // mixing) has no native `sound_decode_url` counterpart yet -
        // `sound_play_url` only supports the audio-device playback path.
        throw const UnsupportedSourceException(
          'UriSource is not supported by openPcmStream yet; use load()/play() instead',
        );
    }
    if (decoded == nullptr) {
      throw UnsupportedSourceException('decode failed: ${_lastError()}');
    }
    try {
      final len = b.decodedLen(decoded);
      final channels = b.decodedChannels(decoded);
      final rate = b.decodedRate(decoded);
      if (len <= 0 || channels <= 0 || rate <= 0) {
        throw const UnsupportedSourceException('decoded zero audio frames');
      }
      final out = malloc<Int16>(len);
      try {
        final n = b.decodedCopy(decoded, 0, out, len);
        final count = n < 0 ? 0 : n;
        final samples = Int16List(count)..setAll(0, out.asTypedList(count));
        return PcmStream(
          samples: samples,
          channels: channels,
          sampleRate: rate,
          chunkFrames: chunkFrames,
          loop: loop,
        );
      } finally {
        malloc.free(out);
      }
    } finally {
      b.decodedFree(decoded);
    }
  }

  @override
  Future<void> dispose() async {
    if (_player != nullptr) {
      _bindings!.playerFree(_player);
      _player = nullptr;
      _generation++;
    }
  }
}

/// A single FFI-backed voice. Created idle; [play] starts a native voice and
/// polls its state to resolve [onComplete]. The native voice is freed as soon
/// as it ends, keeping its final position and duration.
class FfiPlayback implements Playback {
  FfiPlayback._(this._backend, this._source, this._volume, this._looping);

  final FfiBackend _backend;
  final SoundSource _source;

  double _volume;
  bool _looping;
  int _voiceId = 0;
  int _voiceGeneration = 0;
  int _rate = 0;
  int _durationFrames = 0;
  int _positionFrames = 0;
  bool _pauseRequested = false;
  PlaybackState _state = PlaybackState.idle;
  Timer? _poll;
  // Made up front and replaced only once used up, so a future taken before
  // play() - or on a handle that is only ever disposed - still completes.
  Completer<void> _completer = _newCompleter();

  // A voice error completes this with an error; ignore() keeps that from
  // surfacing as an uncaught error when nobody awaits onComplete, while
  // anyone who does still receives it.
  static Completer<void> _newCompleter() => Completer<void>()..future.ignore();

  _Bindings get _b => _backend._bindings!;
  Pointer<Void> get _player => _backend._player;

  @override
  PlaybackState get state => _state;

  @override
  bool get isPlaying => _state == PlaybackState.playing;

  @override
  Future<void> get onComplete => _completer.future;

  @override
  Future<void> play() async {
    if (_state == PlaybackState.disposed) return;
    _freeVoice();
    if (_completer.isCompleted) _completer = _newCompleter();
    _pauseRequested = false;
    _positionFrames = 0;
    _voiceId = _start();
    _voiceGeneration = _backend._generation;
    if (_voiceId == 0) {
      _state = PlaybackState.idle;
      throw PlaybackException('playback failed: ${_backend._lastError()}');
    }
    if (_volume != 1.0) _b.setVolume(_player, _voiceId, _volume);
    _rate = _b.sampleRate(_player, _voiceId);
    _durationFrames = _b.durationFrames(_player, _voiceId);
    // For a UriSource, the native voice starts in a connecting state (rate/
    // duration both 0) until its background thread's first chunk decodes;
    // _checkState's next poll picks up the real state once it does.
    _state = _rate > 0 ? PlaybackState.playing : PlaybackState.connecting;
    _poll = Timer.periodic(
      const Duration(milliseconds: 50),
      (_) => _checkState(),
    );
  }

  int _start() {
    final loopFlag = _looping ? 1 : 0;
    switch (_source) {
      case BytesSource(:final bytes, :final format):
        final buf = malloc<Uint8>(bytes.length);
        final cFormat = format == null ? nullptr : format.toNativeUtf8();
        try {
          buf.asTypedList(bytes.length).setAll(0, bytes);
          // The native side decodes synchronously before returning, so the
          // buffers are safe to free immediately afterward.
          return _b.playBytes(_player, buf, bytes.length, cFormat, loopFlag);
        } finally {
          malloc.free(buf);
          if (cFormat != nullptr) malloc.free(cFormat);
        }
      case FileSource(:final path):
        final cPath = path.toNativeUtf8();
        try {
          return _b.playFile(_player, cPath, loopFlag);
        } finally {
          malloc.free(cPath);
        }
      case UriSource(:final uri, :final headers, :final format):
        final cUrl = uri.toString().toNativeUtf8();
        final cFormat = format == null ? nullptr : format.toNativeUtf8();
        final entries = headers.entries.toList(growable: false);
        final names = malloc<Pointer<Utf8>>(entries.length);
        final values = malloc<Pointer<Utf8>>(entries.length);
        final allocatedStrings = <Pointer<Utf8>>[];
        try {
          for (var i = 0; i < entries.length; i++) {
            final name = entries[i].key.toNativeUtf8();
            final value = entries[i].value.toNativeUtf8();
            allocatedStrings
              ..add(name)
              ..add(value);
            names[i] = name;
            values[i] = value;
          }
          // The native side copies the URL, headers and hint before
          // returning, so the request buffers are safe to free immediately
          // afterward. It only returns 0 for null or non-UTF-8 arguments; a
          // bad URL, HTTP failure or unsupported format surfaces later as
          // the voice's error state, from its background thread.
          return _b.playUrl(
            _player,
            cUrl,
            names,
            values,
            entries.length,
            cFormat,
            loopFlag,
          );
        } finally {
          malloc.free(cUrl);
          if (cFormat != nullptr) malloc.free(cFormat);
          for (final s in allocatedStrings) {
            malloc.free(s);
          }
          malloc.free(names);
          malloc.free(values);
        }
    }
  }

  void _checkState() {
    if (!_hasVoice) return;
    // For `UriSource`, the rate is 0 (and duration/position meaningless)
    // until the background thread's first chunk decodes, and frames decoded
    // so far (hence duration) keep growing after that while the stream
    // downloads - so both are re-read every poll rather than once in
    // `play()`. A no-op re-read for the bytes/file sources, whose rate and
    // total are already known up front.
    final rate = _b.sampleRate(_player, _voiceId);
    final total = _b.durationFrames(_player, _voiceId);
    if (rate >= 0) _rate = rate;
    if (total >= 0) _durationFrames = total;
    final native = _b.voiceState(_player, _voiceId);
    final running =
        native == _statePlaying ||
        native == _statePaused ||
        native == _stateConnecting;
    if (_state == PlaybackState.stopped) {
      // stop() was requested: wait for the thread to exit, then free it
      // without completing onComplete.
      if (!running) _finish(PlaybackState.stopped);
      return;
    }
    switch (native) {
      // The playback thread only notices pause/resume between chunk writes,
      // so its report lags a request; until it catches up, the request wins.
      case _stateConnecting:
        _state = _pauseRequested
            ? PlaybackState.paused
            : PlaybackState.connecting;
      case _statePlaying || _statePaused:
        _state = _pauseRequested ? PlaybackState.paused : PlaybackState.playing;
      case _stateCompleted:
        _finish(PlaybackState.completed);
        if (!_completer.isCompleted) _completer.complete();
      case _stateError:
        // Read the message before _finish frees the voice it belongs to.
        final msg = _b.voiceError(_player, _voiceId);
        final error = PlaybackException(
          msg == nullptr ? 'playback error' : msg.toDartString(),
        );
        _finish(PlaybackState.stopped);
        if (!_completer.isCompleted) _completer.completeError(error);
      case _stateStopped:
        _finish(PlaybackState.stopped);
      default: // -1 unknown
        _finish(PlaybackState.stopped);
    }
  }

  /// Ends the run in [state], keeping the final position, and frees the
  /// native voice so a handle that is never disposed doesn't hold it.
  void _finish(PlaybackState state) {
    _readPosition();
    _freeVoice();
    _state = state;
  }

  @override
  Future<void> stop() async {
    if (!_hasVoice || !_isRunning) return;
    _b.stop(_player, _voiceId);
    // The poll keeps running until the thread exits, then frees the voice.
    _state = PlaybackState.stopped;
  }

  /// Started and not yet ended: the states [stop] moves to stopped.
  bool get _isRunning =>
      _state == PlaybackState.playing ||
      _state == PlaybackState.paused ||
      _state == PlaybackState.connecting;

  @override
  Future<void> setVolume(double volume) async {
    _volume = volume;
    if (_hasVoice) _b.setVolume(_player, _voiceId, volume);
  }

  @override
  Future<void> setLooping(bool looping) async {
    _looping = looping;
    if (_hasVoice) _b.setLoop(_player, _voiceId, looping ? 1 : 0);
  }

  @override
  Future<void> pause() async {
    if (!_hasVoice) return;
    if (_state != PlaybackState.playing && _state != PlaybackState.connecting) {
      return;
    }
    _b.pause(_player, _voiceId);
    _pauseRequested = true;
    _state = PlaybackState.paused;
  }

  @override
  Future<void> resume() async {
    if (!_hasVoice || _state != PlaybackState.paused) return;
    _b.resume(_player, _voiceId);
    _pauseRequested = false;
    _state = _rate > 0 ? PlaybackState.playing : PlaybackState.connecting;
  }

  @override
  Future<void> seek(Duration position) async {
    // _rate is 0 while a URL voice is still connecting (the stream's rate is
    // unknown until the first chunk decodes), so a seek during that window is
    // a no-op rather than a guess at the frame index.
    if (_hasVoice && _rate > 0) {
      final frame = position.inMicroseconds * _rate ~/ 1000000;
      _b.seek(_player, _voiceId, frame < 0 ? 0 : frame);
    }
  }

  @override
  Duration get position {
    _readPosition();
    if (_rate <= 0) return Duration.zero;
    return Duration(microseconds: _positionFrames * 1000000 ~/ _rate);
  }

  @override
  Duration? get duration {
    if (_durationFrames <= 0 || _rate <= 0) return null;
    return Duration(microseconds: _durationFrames * 1000000 ~/ _rate);
  }

  /// Whether this handle still has a native voice. After the backend frees
  /// its player the id is dropped instead of used: it is meaningless, or
  /// once a new player is created, names another playback's voice.
  bool get _hasVoice {
    if (_voiceId != 0 && _voiceGeneration != _backend._generation) {
      _voiceId = 0;
      _poll?.cancel();
      _poll = null;
      if (_isRunning) _state = PlaybackState.stopped;
    }
    return _voiceId != 0;
  }

  /// Updates the last known position while the native voice exists.
  void _readPosition() {
    if (!_hasVoice) return;
    final frames = _b.position(_player, _voiceId);
    if (frames >= 0) _positionFrames = frames;
  }

  void _freeVoice() {
    _poll?.cancel();
    _poll = null;
    if (_hasVoice) {
      _b.voiceFree(_player, _voiceId);
      _voiceId = 0;
    }
  }

  @override
  Future<void> dispose() async {
    _freeVoice();
    _state = PlaybackState.disposed;
    if (!_completer.isCompleted) _completer.complete();
  }
}
