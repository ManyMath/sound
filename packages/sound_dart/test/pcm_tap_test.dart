@TestOn('vm')
library;

import 'dart:async';
import 'dart:typed_data';

import 'package:sound_dart/sound_dart.dart';
import 'package:sound_dart/src/backends/native/native_backend_ffi.dart';
import 'package:test/test.dart';

/// Builds a 16-bit PCM WAV whose every sample equals [value].
Uint8List constWav({
  int value = 1000,
  int sampleRate = 8000,
  int channels = 1,
  int frames = 200,
}) {
  final dataBytes = frames * channels * 2;
  final out = Uint8List(44 + dataBytes);
  final view = ByteData.sublistView(out);
  var o = 0;
  void ascii(String s) {
    for (final c in s.codeUnits) {
      out[o++] = c;
    }
  }

  void u32(int v) {
    view.setUint32(o, v, Endian.little);
    o += 4;
  }

  void u16(int v) {
    view.setUint16(o, v, Endian.little);
    o += 2;
  }

  ascii('RIFF');
  u32(36 + dataBytes);
  ascii('WAVE');
  ascii('fmt ');
  u32(16);
  u16(1);
  u16(channels);
  u32(sampleRate);
  u32(sampleRate * channels * 2);
  u16(channels * 2);
  u16(16);
  ascii('data');
  u32(dataBytes);
  for (var i = 0; i < frames * channels; i++) {
    view.setInt16(o, value, Endian.little);
    o += 2;
  }
  return out;
}

void main() {
  group('PCM tap via SilentBackend', () {
    setUp(() => Sound.reset());
    tearDown(() => Sound.reset());

    SilentBackend pin({
      Duration duration = const Duration(milliseconds: 100),
      int rate = 8000,
      int channels = 1,
    }) {
      final silent = SilentBackend(
        simulatedDuration: duration,
        pcmSampleRate: rate,
        pcmChannels: channels,
      );
      Sound.registerBackend(silent, makeActive: true);
      return silent;
    }

    test('yields silence of the configured format', () async {
      pin();
      final s = await Sound.openPcmStream(SoundSource.bytes(Uint8List(0)));
      expect(s.sampleRate, 8000);
      expect(s.channels, 1);
      expect(s.buffer.length, 800); // 8000 Hz * 0.1 s, mono
      expect(s.frameCount, 800);
      expect(s.buffer.every((v) => v == 0), isTrue);
      await s.dispose();
    });

    test('streams the silence in chunks and then closes', () async {
      pin();
      final s = await Sound.openPcmStream(
        SoundSource.bytes(Uint8List(0)),
        chunkFrames: 200,
      );
      final collected = <int>[];
      var chunks = 0;
      final sub = s.frames.listen((c) {
        chunks++;
        collected.addAll(c);
      });
      await s.start();
      await s.onDone;
      await sub.cancel();
      expect(chunks, 4); // 800 frames / 200 per chunk
      expect(collected.length, 800);
      expect(collected.every((v) => v == 0), isTrue);
    });

    test('an empty buffer closes immediately on start', () async {
      pin(duration: Duration.zero);
      final s = await Sound.openPcmStream(SoundSource.bytes(Uint8List(0)));
      expect(s.buffer, isEmpty);
      var chunks = 0;
      s.frames.listen((_) => chunks++);
      await s.start();
      await s.onDone;
      expect(chunks, 0);
    });

    test('loop keeps emitting past the buffer length until stopped', () async {
      pin(duration: const Duration(milliseconds: 25)); // 200 frames
      final s = await Sound.openPcmStream(
        SoundSource.bytes(Uint8List(0)),
        chunkFrames: 100,
        loop: true,
      );
      var total = 0;
      final sub = s.frames.listen((c) => total += c.length);
      await s.start();
      await Future<void>.delayed(const Duration(milliseconds: 150));
      await s.stop();
      await sub.cancel();
      expect(total, greaterThan(200)); // more than one pass through the buffer
    });

    test('stop closes frames without completing onDone', () async {
      pin(duration: const Duration(seconds: 1));
      final s = await Sound.openPcmStream(
        SoundSource.bytes(Uint8List(0)),
        chunkFrames: 100,
      );
      var natural = false;
      unawaited(s.onDone.then((_) => natural = true));
      final closed = Completer<void>();
      s.frames.listen(null, onDone: closed.complete);
      await s.start();
      await s.stop();
      await closed.future.timeout(const Duration(seconds: 1));
      await Future<void>.delayed(const Duration(milliseconds: 50));
      expect(natural, isFalse);
    });

    test('openPcmStream records the source on the silent backend', () async {
      final silent = pin();
      await Sound.openPcmStream(SoundSource.file('clip.wav'));
      expect(silent.loaded, hasLength(1));
      expect(silent.loaded.single, isA<FileSource>());
    });
  });

  group(
    'PCM tap via FFI (real decode)',
    () {
      setUp(() => Sound.reset());
      tearDown(() => Sound.reset());

      test('decodes a WAV into interleaved S16 frames', () async {
        Sound.registerBackend(FfiBackend(), makeActive: true);
        final s = await Sound.openPcmStream(
          SoundSource.bytes(constWav(value: 1234, frames: 200), format: 'wav'),
        );
        expect(s.sampleRate, 8000);
        expect(s.channels, 1);
        expect(s.frameCount, 200);
        expect(s.buffer.every((v) => v == 1234), isTrue);
        await s.dispose();
      });

      test('streams the decoded frames and completes', () async {
        Sound.registerBackend(FfiBackend(), makeActive: true);
        final s = await Sound.openPcmStream(
          SoundSource.bytes(constWav(value: 50, frames: 256), format: 'wav'),
          chunkFrames: 64,
        );
        final collected = <int>[];
        final sub = s.frames.listen(collected.addAll);
        await s.start();
        await s.onDone;
        await sub.cancel();
        expect(collected.length, 256);
        expect(collected.every((v) => v == 50), isTrue);
      });

      test('decodes a stereo WAV preserving channel interleave', () async {
        Sound.registerBackend(FfiBackend(), makeActive: true);
        final s = await Sound.openPcmStream(
          SoundSource.bytes(
            constWav(value: 7, frames: 100, channels: 2),
            format: 'wav',
          ),
        );
        expect(s.channels, 2);
        expect(s.frameCount, 100);
        expect(s.buffer.length, 200); // 100 frames * 2 channels
        await s.dispose();
      });

      test('a garbage source surfaces a clear error', () async {
        Sound.registerBackend(FfiBackend(), makeActive: true);
        expect(
          () => Sound.openPcmStream(SoundSource.bytes(Uint8List(64))),
          throwsA(isA<UnsupportedSourceException>()),
        );
      });
    },
    skip: NativeLibrary.open() != null ? false : 'sound_cli library not built',
  );
}
