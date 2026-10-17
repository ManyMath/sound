@TestOn('browser')
library;

import 'dart:js_interop';
import 'dart:math';
import 'dart:typed_data';

import 'package:sound_dart/sound_dart.dart';
import 'package:sound_dart/src/backends/native/native_backend_web.dart';
import 'package:test/test.dart';
import 'package:web/web.dart' as web;

/// Builds a 16-bit mono PCM WAV of a quiet tone.
Uint8List toneWav({int sampleRate = 8000, int milliseconds = 200}) {
  final frames = sampleRate * milliseconds ~/ 1000;
  final dataBytes = frames * 2;
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
  u16(1);
  u32(sampleRate);
  u32(sampleRate * 2);
  u16(2);
  u16(16);
  ascii('data');
  u32(dataBytes);
  for (var i = 0; i < frames; i++) {
    final v = (0.01 * sin(2 * pi * 440 * i / sampleRate) * 32767).round();
    view.setInt16(o, v, Endian.little);
    o += 2;
  }
  return out;
}

void main() {
  setUp(() => Sound.reset());
  tearDown(() => Sound.reset());

  test('selects the WebAudio backend', () {
    expect(Sound.backend.name, 'webaudio');
  });

  test('play() resumes a suspended AudioContext', () async {
    final context = web.AudioContext();
    addTearDown(() => context.close().toDart);
    await context.suspend().toDart;
    final buffer = await context.decodeAudioData(toneWav().buffer.toJS).toDart;
    final pb = WebAudioPlayback(context, buffer, 1.0, false);
    await pb.play();
    await pb.onComplete.timeout(const Duration(seconds: 3));
    expect(context.state, 'running');
    await pb.dispose();
  });

  test('a looping voice reports its position within the clip', () async {
    final pb = await Sound.play(
      SoundSource.bytes(toneWav(), format: 'wav'),
      loop: true,
    );
    final duration = pb.duration!;
    await Future<void>.delayed(const Duration(milliseconds: 700));
    expect(pb.position, lessThan(duration));
    await pb.pause();
    final at = pb.position;
    expect(at, lessThan(duration));
    await pb.resume();
    expect((pb.position - at).inMilliseconds.abs(), lessThan(50));
    await Future<void>.delayed(const Duration(milliseconds: 300));
    await pb.setLooping(false);
    expect(pb.position, lessThan(duration));
    await pb.onComplete.timeout(const Duration(seconds: 1));
    await pb.dispose();
  });

  test('undecodable bytes throw an UnsupportedSourceException', () async {
    await expectLater(
      Sound.load(SoundSource.bytes(Uint8List(64))),
      throwsA(isA<UnsupportedSourceException>()),
    );
  });

  test('a failed fetch throws a PlaybackException', () async {
    await expectLater(
      Sound.load(SoundSource.uri(Uri.parse('http://127.0.0.1:1/tone.wav'))),
      throwsA(isA<PlaybackException>()),
    );
  });

  test('volumes below zero or NaN clamp to silence', () async {
    final pb = await Sound.load(
      SoundSource.bytes(toneWav(), format: 'wav'),
      volume: -1,
    );
    expect(pb.volume, 0.0);
    await pb.setVolume(0.5);
    expect(pb.volume, 0.5);
    await pb.setVolume(-0.5);
    expect(pb.volume, 0.0);
    await pb.setVolume(double.nan);
    expect(pb.volume, 0.0);
    await pb.dispose();
  });

  test('an onComplete taken before play() completes', () async {
    final pb = await Sound.load(SoundSource.bytes(toneWav(), format: 'wav'));
    final done = pb.onComplete;
    await pb.play();
    await done.timeout(const Duration(seconds: 3));
    expect(pb.state, PlaybackState.completed);
    await pb.dispose();
  });

  test('stop while paused ends the run; resume does not revive it', () async {
    final pb = await Sound.play(
      SoundSource.bytes(toneWav(milliseconds: 2000), format: 'wav'),
    );
    await pb.pause();
    await pb.stop();
    expect(pb.state, PlaybackState.stopped);
    await pb.resume();
    expect(pb.state, PlaybackState.stopped);
    await pb.dispose();
  });

  test('dispose completes onComplete of a never-played handle', () async {
    final pb = await Sound.load(SoundSource.bytes(toneWav(), format: 'wav'));
    await pb.dispose();
    await pb.onComplete.timeout(const Duration(seconds: 1));
  });
}
