import 'dart:io';
import 'dart:math';
import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:sound_flutter/sound_flutter.dart';

/// Generous: emulators and loaded hosts open audio streams slowly.
const _slow = Duration(seconds: 20);

/// Builds a valid 16-bit PCM mono WAV containing a sine tone.
Uint8List buildSineWav({
  int sampleRate = 8000,
  int milliseconds = 150,
  double freq = 440,
}) {
  final frames = sampleRate * milliseconds ~/ 1000;
  final dataBytes = frames * 2;
  final bytes = Uint8List(44 + dataBytes);
  final view = ByteData.sublistView(bytes);
  void ascii(int at, String s) => bytes.setAll(at, s.codeUnits);
  ascii(0, 'RIFF');
  view.setUint32(4, 36 + dataBytes, Endian.little);
  ascii(8, 'WAVE');
  ascii(12, 'fmt ');
  view.setUint32(16, 16, Endian.little);
  view.setUint16(20, 1, Endian.little); // PCM
  view.setUint16(22, 1, Endian.little); // mono
  view.setUint32(24, sampleRate, Endian.little);
  view.setUint32(28, sampleRate * 2, Endian.little);
  view.setUint16(32, 2, Endian.little);
  view.setUint16(34, 16, Endian.little);
  ascii(36, 'data');
  view.setUint32(40, dataBytes, Endian.little);
  for (var i = 0; i < frames; i++) {
    final v = (0.3 * sin(2 * pi * freq * i / sampleRate) * 32767).round();
    view.setInt16(44 + i * 2, v, Endian.little);
  }
  return bytes;
}

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  setUpAll(() async {
    expect(await SoundFlutter.ensureInitialized(), 'ffi');
  });

  test('native library loads and the ffi backend is active', () {
    expect(Sound.backend.name, 'ffi');
    expect(Sound.backend.isAvailable, isTrue);
  });

  test('playBytes plays a WAV to completion', () async {
    final pb = await Sound.playBytes(buildSineWav(), format: 'wav');
    expect(pb.isPlaying, isTrue);
    await pb.onComplete.timeout(_slow);
    expect(pb.state, PlaybackState.completed);
    await pb.dispose();
  });

  test('pause holds position, resume completes', () async {
    final pb = await Sound.playBytes(
      buildSineWav(sampleRate: 44100, milliseconds: 1500),
      format: 'wav',
    );
    await Future<void>.delayed(const Duration(milliseconds: 300));
    await pb.pause();
    expect(pb.state, PlaybackState.paused);
    // Let any in-flight write land before sampling the position.
    await Future<void>.delayed(const Duration(milliseconds: 200));
    final held = pb.position;
    await Future<void>.delayed(const Duration(milliseconds: 500));
    expect((pb.position - held).inMilliseconds.abs(), lessThan(50));
    await pb.resume();
    await pb.onComplete.timeout(_slow);
    expect(pb.state, PlaybackState.completed);
    await pb.dispose();
  });

  test('seek near the end completes quickly', () async {
    final pb = await Sound.playBytes(
      buildSineWav(milliseconds: 6000),
      format: 'wav',
    );
    await pb.seek(const Duration(milliseconds: 5900));
    // Far less than the 6s a full play would take.
    await pb.onComplete.timeout(const Duration(seconds: 4));
    expect(pb.state, PlaybackState.completed);
    await pb.dispose();
  });

  test('stop ends playback early', () async {
    final pb = await Sound.playBytes(
      buildSineWav(milliseconds: 5000),
      format: 'wav',
    );
    await Future<void>.delayed(const Duration(milliseconds: 200));
    await pb.stop();
    expect(pb.state, PlaybackState.stopped);
    await pb.dispose();
  });

  test('setVolume and setLooping do not error', () async {
    final pb = await Sound.playBytes(
      buildSineWav(milliseconds: 1000),
      format: 'wav',
    );
    await pb.setVolume(0.25);
    await pb.setLooping(true);
    await pb.setLooping(false);
    await pb.setVolume(0.5);
    await pb.onComplete.timeout(_slow);
    expect(pb.state, PlaybackState.completed);
    await pb.dispose();
  });

  test('two voices play concurrently', () async {
    final voices = await Future.wait([
      Sound.playBytes(buildSineWav(freq: 330), format: 'wav'),
      Sound.playBytes(buildSineWav(freq: 550), format: 'wav'),
    ]);
    expect(voices.every((v) => v.isPlaying), isTrue);
    await Future.wait(voices.map((v) => v.onComplete.timeout(_slow)));
    expect(voices.every((v) => v.state == PlaybackState.completed), isTrue);
    for (final v in voices) {
      await v.dispose();
    }
  });

  test('playUrl fetches from a loopback server and plays', () async {
    final wav = buildSineWav();
    final server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
    addTearDown(() => server.close(force: true));
    server.listen((req) {
      req.response
        ..headers.contentType = ContentType('audio', 'wav')
        ..add(wav);
      req.response.close();
    });

    final pb = await Sound.playUrl(
      'http://${server.address.host}:${server.port}/tone.wav',
    );
    await pb.onComplete.timeout(_slow);
    expect(pb.state, PlaybackState.completed);
    await pb.dispose();
  });

  test('garbage bytes give an exception', () async {
    final garbage = Uint8List.fromList(List.generate(256, (i) => i ^ 0x5a));
    await expectLater(() async {
      final pb = await Sound.playBytes(garbage, format: 'wav');
      await pb.onComplete.timeout(_slow);
    }(), throwsA(isA<Exception>()));
  });
}
