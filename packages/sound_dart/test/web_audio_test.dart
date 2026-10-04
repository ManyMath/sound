@TestOn('browser')
library;

import 'dart:math';
import 'dart:typed_data';

import 'package:sound_dart/sound_dart.dart';
import 'package:test/test.dart';

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

  test('an onComplete taken before play() completes', () async {
    final pb = await Sound.load(SoundSource.bytes(toneWav(), format: 'wav'));
    final done = pb.onComplete;
    await pb.play();
    await done.timeout(const Duration(seconds: 3));
    expect(pb.state, PlaybackState.completed);
    await pb.dispose();
  });

  test('dispose completes onComplete of a never-played handle', () async {
    final pb = await Sound.load(SoundSource.bytes(toneWav(), format: 'wav'));
    await pb.dispose();
    await pb.onComplete.timeout(const Duration(seconds: 1));
  });
}
