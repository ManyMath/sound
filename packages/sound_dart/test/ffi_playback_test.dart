@TestOn('vm')
library;

import 'dart:async';
import 'dart:io';
import 'dart:math';
import 'dart:typed_data';

import 'package:sound_dart/sound_dart.dart';
import 'package:sound_dart/src/backends/native/native_backend_ffi.dart';
import 'package:test/test.dart';

/// Builds a valid 16-bit PCM WAV containing a short sine tone.
Uint8List buildSineWav({
  int sampleRate = 8000,
  int milliseconds = 150,
  double freq = 440,
  int channels = 1,
}) {
  final frames = sampleRate * milliseconds ~/ 1000;
  final dataBytes = frames * channels * 2;
  final blockAlign = channels * 2;
  final out = BytesData(44 + dataBytes);
  out.setAscii('RIFF');
  out.u32(36 + dataBytes);
  out.setAscii('WAVE');
  out.setAscii('fmt ');
  out.u32(16); // PCM fmt chunk size
  out.u16(1); // PCM
  out.u16(channels);
  out.u32(sampleRate);
  out.u32(sampleRate * blockAlign); // byte rate
  out.u16(blockAlign);
  out.u16(16); // bits per sample
  out.setAscii('data');
  out.u32(dataBytes);
  for (var i = 0; i < frames; i++) {
    final v = (0.3 * sin(2 * pi * freq * i / sampleRate) * 32767).round();
    for (var c = 0; c < channels; c++) {
      out.i16(v);
    }
  }
  return out.bytes;
}

/// Tiny little-endian byte writer.
class BytesData {
  BytesData(int size) : bytes = Uint8List(size), _view = ByteData(0) {
    _view = ByteData.sublistView(bytes);
  }
  final Uint8List bytes;
  ByteData _view;
  int _offset = 0;

  void setAscii(String s) {
    for (final c in s.codeUnits) {
      bytes[_offset++] = c;
    }
  }

  void u32(int v) {
    _view.setUint32(_offset, v, Endian.little);
    _offset += 4;
  }

  void u16(int v) {
    _view.setUint16(_offset, v, Endian.little);
    _offset += 2;
  }

  void i16(int v) {
    _view.setInt16(_offset, v, Endian.little);
    _offset += 2;
  }
}

void main() {
  final available = NativeLibrary.open() != null;

  group('FFI playback (end-to-end)', () {
    setUp(() => Sound.reset());
    tearDown(() => Sound.reset());

    test('plays a WAV to natural completion', () async {
      final backend = FfiBackend();
      expect(backend.isAvailable, isTrue);
      Sound.registerBackend(backend, makeActive: true);

      final playback = await Sound.playBytes(buildSineWav(), format: 'wav');
      expect(playback.isPlaying, isTrue);
      await playback.onComplete.timeout(const Duration(seconds: 5));
      expect(playback.state, PlaybackState.completed);
      await playback.dispose();
    });

    test('plays at reduced volume to completion', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final playback = await Sound.playBytes(
        buildSineWav(milliseconds: 120),
        format: 'wav',
        volume: 0.25,
      );
      await playback.setVolume(0.5);
      await playback.onComplete.timeout(const Duration(seconds: 5));
      expect(playback.state, PlaybackState.completed);
      await playback.dispose();
    });

    test('volume reports the loaded and last-set value', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final playback = await Sound.load(
        SoundSource.bytes(buildSineWav(milliseconds: 80), format: 'wav'),
        volume: 0.25,
      );
      expect(playback.volume, 0.25);
      await playback.setVolume(0.5);
      expect(playback.volume, 0.5);
      await playback.setVolume(-1);
      expect(playback.volume, 0.0);
      await playback.dispose();
    });

    test('looping keeps playing past the end until stopped', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final playback = await Sound.playBytes(
        buildSineWav(milliseconds: 80),
        format: 'wav',
        loop: true,
      );
      // Well past one pass; a non-looping voice would have completed by now.
      await Future<void>.delayed(const Duration(milliseconds: 300));
      expect(playback.state, PlaybackState.playing);
      await playback.stop();
      expect(playback.state, PlaybackState.stopped);
      await playback.dispose();
    });

    test('plays multiple voices concurrently to completion', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final voices = await Future.wait([
        Sound.playBytes(
          buildSineWav(milliseconds: 150, freq: 330),
          format: 'wav',
        ),
        Sound.playBytes(
          buildSineWav(milliseconds: 150, freq: 440),
          format: 'wav',
        ),
        Sound.playBytes(
          buildSineWav(milliseconds: 150, freq: 550),
          format: 'wav',
        ),
      ]);
      expect(voices.every((v) => v.isPlaying), isTrue);
      await Future.wait(
        voices.map((v) => v.onComplete.timeout(const Duration(seconds: 5))),
      );
      expect(voices.every((v) => v.state == PlaybackState.completed), isTrue);
      for (final v in voices) {
        await v.dispose();
      }
    });

    test('reports duration and a non-zero advancing position', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final pb = await Sound.playBytes(
        buildSineWav(sampleRate: 44100, milliseconds: 1500),
        format: 'wav',
      );
      expect(pb.duration, isNotNull);
      expect((pb.duration!.inMilliseconds - 1500).abs(), lessThan(60));
      // Allow for stream-open latency before the first frames are written.
      await Future<void>.delayed(const Duration(milliseconds: 400));
      expect(pb.position, greaterThan(Duration.zero));
      await pb.stop();
      await pb.dispose();
    });

    test('pause holds position, resume continues to completion', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final pb = await Sound.playBytes(
        buildSineWav(sampleRate: 44100, milliseconds: 900),
        format: 'wav',
      );
      await Future<void>.delayed(const Duration(milliseconds: 150));
      await pb.pause();
      expect(pb.state, PlaybackState.paused);
      // Let any in-flight write land, then confirm position is frozen.
      await Future<void>.delayed(const Duration(milliseconds: 80));
      final p1 = pb.position;
      await Future<void>.delayed(const Duration(milliseconds: 250));
      expect((pb.position - p1).inMilliseconds.abs(), lessThan(30));
      await pb.resume();
      await pb.onComplete.timeout(const Duration(seconds: 5));
      expect(pb.state, PlaybackState.completed);
      await pb.dispose();
    });

    test('pause and resume hold across polls', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      // At 8 kHz each native chunk write lasts ~256 ms, so the playback
      // thread notices pause/resume well after several 50 ms polls.
      final pb = await Sound.playBytes(
        buildSineWav(sampleRate: 8000, milliseconds: 3000),
        format: 'wav',
      );
      await Future<void>.delayed(const Duration(milliseconds: 300));
      Future<Set<PlaybackState>> sample() async {
        final seen = <PlaybackState>{};
        for (var i = 0; i < 8; i++) {
          await Future<void>.delayed(const Duration(milliseconds: 30));
          seen.add(pb.state);
        }
        return seen;
      }

      await pb.pause();
      expect(await sample(), {PlaybackState.paused});
      await pb.resume();
      expect(await sample(), {PlaybackState.playing});
      await pb.dispose();
    });

    test('seek near the end finishes quickly', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final pb = await Sound.playBytes(
        buildSineWav(milliseconds: 4000),
        format: 'wav',
      );
      await pb.seek(const Duration(milliseconds: 3900));
      await pb.onComplete.timeout(const Duration(seconds: 2));
      expect(pb.state, PlaybackState.completed);
      await pb.dispose();
    });

    test('playUrl fetches over HTTP and plays', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final wav = buildSineWav(milliseconds: 120);
      final server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
      server.listen((req) {
        req.response
          ..headers.contentType = ContentType('audio', 'wav')
          ..add(wav);
        req.response.close();
      });
      addTearDown(() => server.close(force: true));

      final url = 'http://${server.address.host}:${server.port}/tone.wav';
      final pb = await Sound.playUrl(url);
      await pb.onComplete.timeout(const Duration(seconds: 5));
      expect(pb.state, PlaybackState.completed);
      await pb.dispose();
    });

    test('playUrl sends custom headers with the request', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final wav = buildSineWav(milliseconds: 120);
      String? receivedAuth;
      final server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
      server.listen((req) {
        receivedAuth = req.headers.value('authorization');
        req.response
          ..headers.contentType = ContentType('audio', 'wav')
          ..add(wav);
        req.response.close();
      });
      addTearDown(() => server.close(force: true));

      final url = 'http://${server.address.host}:${server.port}/tone.wav';
      final pb = await Sound.playUrl(
        url,
        headers: {'Authorization': 'Bearer test-token-123'},
      );
      await pb.onComplete.timeout(const Duration(seconds: 5));
      expect(receivedAuth, 'Bearer test-token-123');
      await pb.dispose();
    });

    test(
      'a UriSource voice reports connecting before the format is known',
      () async {
        Sound.registerBackend(FfiBackend(), makeActive: true);
        final wav = buildSineWav(milliseconds: 120);
        final gate = Completer<void>();
        final server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
        server.listen((req) async {
          // Holds the response open so `play()` observably returns before
          // the stream's format is known, proving it never blocked on this
          // request.
          await gate.future;
          req.response
            ..headers.contentType = ContentType('audio', 'wav')
            ..add(wav);
          await req.response.close();
        });
        addTearDown(() => server.close(force: true));

        final url = 'http://${server.address.host}:${server.port}/tone.wav';
        final pb = await Sound.load(SoundSource.uri(Uri.parse(url)));
        await pb.play();
        expect(pb.state, PlaybackState.connecting);
        expect(pb.duration, isNull);

        gate.complete();
        await pb.onComplete.timeout(const Duration(seconds: 5));
        expect(pb.state, PlaybackState.completed);
        await pb.dispose();
      },
    );

    test('playUrl surfaces a clear error on HTTP failure', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
      server.listen((req) {
        req.response.statusCode = 404;
        req.response.close();
      });
      addTearDown(() => server.close(force: true));
      final url = 'http://${server.address.host}:${server.port}/missing.wav';
      // Connecting, probing, and decoding the first chunk all happen on a
      // background thread so playUrl() itself never blocks on network I/O
      // (and so never fails synchronously) - a connect/HTTP-status failure
      // surfaces through onComplete instead, the same as a failure partway
      // through an already-playing stream.
      final pb = await Sound.playUrl(url);
      await expectLater(pb.onComplete, throwsA(isA<PlaybackException>()));
    });

    test('a voice error nobody awaits is not an uncaught error', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
      server.listen((req) {
        req.response.statusCode = 404;
        req.response.close();
      });
      addTearDown(() => server.close(force: true));
      final url = 'http://${server.address.host}:${server.port}/missing.wav';
      final uncaught = <Object>[];
      await runZonedGuarded(() async {
        // Fire and forget: nothing listens to onComplete.
        final pb = await Sound.playUrl(url);
        final deadline = DateTime.now().add(const Duration(seconds: 5));
        while (pb.state == PlaybackState.connecting &&
            DateTime.now().isBefore(deadline)) {
          await Future<void>.delayed(const Duration(milliseconds: 20));
        }
        await Future<void>.delayed(const Duration(milliseconds: 50));
        expect(pb.state, PlaybackState.stopped);
        await pb.dispose();
      }, (e, _) => uncaught.add(e));
      expect(uncaught, isEmpty);
    });

    test('replaying the same handle restarts and completes', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final pb = await Sound.playBytes(
        buildSineWav(milliseconds: 80),
        format: 'wav',
      );
      await pb.onComplete.timeout(const Duration(seconds: 5));
      // Play the same handle again; it should restart and complete.
      await pb.play();
      expect(pb.isPlaying, isTrue);
      await pb.onComplete.timeout(const Duration(seconds: 5));
      expect(pb.state, PlaybackState.completed);
      await pb.dispose();
    });

    test('an onComplete taken before play() completes', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final pb = await Sound.load(
        SoundSource.bytes(buildSineWav(milliseconds: 80), format: 'wav'),
      );
      final done = pb.onComplete;
      await pb.play();
      await done.timeout(const Duration(seconds: 5));
      expect(pb.state, PlaybackState.completed);
      await pb.dispose();
    });

    test('dispose completes onComplete of a never-played handle', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final pb = await Sound.load(
        SoundSource.bytes(buildSineWav(milliseconds: 80), format: 'wav'),
      );
      await pb.dispose();
      await pb.onComplete.timeout(const Duration(seconds: 1));
    });

    test('dispose is idempotent and stop-after-dispose is safe', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final pb = await Sound.playBytes(
        buildSineWav(milliseconds: 80),
        format: 'wav',
      );
      await pb.dispose();
      await pb.dispose(); // no throw
      await pb.stop(); // no throw
      expect(pb.state, PlaybackState.disposed);
    });

    test('plays a stereo WAV to completion', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final pb = await Sound.playBytes(
        buildSineWav(sampleRate: 44100, milliseconds: 150, channels: 2),
        format: 'wav',
      );
      await pb.onComplete.timeout(const Duration(seconds: 5));
      expect(pb.state, PlaybackState.completed);
      await pb.dispose();
    });

    test('stop while paused stops; resume does not revive it', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final pb = await Sound.playBytes(
        buildSineWav(milliseconds: 2000),
        format: 'wav',
      );
      await Future<void>.delayed(const Duration(milliseconds: 100));
      await pb.pause();
      await pb.stop();
      expect(pb.state, PlaybackState.stopped);
      await pb.resume();
      expect(pb.state, PlaybackState.stopped);
      await pb.dispose();
    });

    test('pause/resume after completion leave it completed', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final pb = await Sound.playBytes(
        buildSineWav(milliseconds: 80),
        format: 'wav',
      );
      await pb.onComplete.timeout(const Duration(seconds: 5));
      await pb.pause();
      expect(pb.state, PlaybackState.completed);
      await pb.resume();
      expect(pb.state, PlaybackState.completed);
      await pb.dispose();
    });

    test('stop while connecting stops', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final gate = Completer<void>();
      final server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
      server.listen((req) async {
        await gate.future;
        await req.response.close();
      });
      addTearDown(() => server.close(force: true));
      addTearDown(() {
        if (!gate.isCompleted) gate.complete();
      });
      final url = 'http://${server.address.host}:${server.port}/tone.wav';
      final pb = await Sound.playUrl(url);
      expect(pb.state, PlaybackState.connecting);
      await pb.stop();
      expect(pb.state, PlaybackState.stopped);
      await pb.dispose();
    });

    test('a finished voice keeps its position and duration', () async {
      final backend = FfiBackend();
      Sound.registerBackend(backend, makeActive: true);
      final pb = await Sound.playBytes(
        buildSineWav(milliseconds: 120),
        format: 'wav',
      );
      await pb.onComplete.timeout(const Duration(seconds: 5));
      final duration = pb.duration;
      expect(duration, isNotNull);
      expect(pb.position, duration);
      // The voice was freed on completion, so freeing the player under it
      // changes nothing the handle reports.
      await backend.dispose();
      expect(pb.position, duration);
      expect(pb.duration, duration);
      await pb.dispose();
    });

    test('a stopped voice is freed once its thread exits', () async {
      final backend = FfiBackend();
      Sound.registerBackend(backend, makeActive: true);
      final pb = await Sound.playBytes(
        buildSineWav(sampleRate: 44100, milliseconds: 2000),
        format: 'wav',
      );
      await Future<void>.delayed(const Duration(milliseconds: 300));
      await pb.stop();
      await Future<void>.delayed(const Duration(milliseconds: 300));
      final at = pb.position;
      expect(at, greaterThan(Duration.zero));
      await backend.dispose();
      expect(pb.position, at);
      expect(pb.state, PlaybackState.stopped);
      await pb.dispose();
    });

    test('a handle outliving its player leaves newer voices alone', () async {
      final backend = FfiBackend();
      Sound.registerBackend(backend, makeActive: true);
      final old = await Sound.playBytes(
        buildSineWav(milliseconds: 3000),
        format: 'wav',
      );
      await backend.dispose();
      // Loading again creates a new player, whose voice ids start over.
      final fresh = await Sound.playBytes(
        buildSineWav(milliseconds: 3000),
        format: 'wav',
      );
      await Future<void>.delayed(const Duration(milliseconds: 150));
      expect(old.state, PlaybackState.stopped);
      await old.stop();
      await old.dispose();
      await Future<void>.delayed(const Duration(milliseconds: 150));
      expect(fresh.state, PlaybackState.playing);
      await fresh.dispose();
    });

    test('stop ends playback early', () async {
      Sound.registerBackend(FfiBackend(), makeActive: true);
      final playback = await Sound.playBytes(
        buildSineWav(milliseconds: 2000),
        format: 'wav',
      );
      await Future<void>.delayed(const Duration(milliseconds: 100));
      await playback.stop();
      expect(playback.state, PlaybackState.stopped);
      await playback.dispose();
    });
  }, skip: available ? false : 'sound_cli library not built');
}
