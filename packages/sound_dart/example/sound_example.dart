// Plays audio from the command line using `sound_dart` (pure Dart, no Flutter).
//
// Build the native library first:
//   cargo build --manifest-path packages/sound_flutter/rust/Cargo.toml
// then, from the repo root:
//   dart run packages/sound_dart/example/sound_example.dart           # a 440 Hz tone
//   dart run packages/sound_dart/example/sound_example.dart chime.wav # a WAV file
//
// If the library is elsewhere, point to it with SOUND_DART_LIB=/path/to/lib.

import 'dart:math';
import 'dart:typed_data';

import 'package:sound_dart/sound_dart.dart';

Future<void> main(List<String> args) async {
  print('Active backend: ${Sound.backend.name}');

  final Playback playback;
  if (args.isNotEmpty) {
    print('Playing file: ${args.first}');
    playback = await Sound.playFile(args.first);
  } else {
    print('Playing a 440 Hz tone for 1 second...');
    playback = await Sound.playBytes(_sineWav(), format: 'wav');
  }

  await playback.onComplete;
  print('Done (${playback.state.name}).');
  await playback.dispose();
}

/// Builds a 1-second 440 Hz 16-bit mono WAV in memory.
Uint8List _sineWav({int sampleRate = 44100, double freq = 440}) {
  final frames = sampleRate;
  final dataBytes = frames * 2;
  final bytes = Uint8List(44 + dataBytes);
  final view = ByteData.sublistView(bytes);
  var o = 0;
  void ascii(String s) {
    for (final c in s.codeUnits) {
      bytes[o++] = c;
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
  u16(1); // PCM
  u16(1); // mono
  u32(sampleRate);
  u32(sampleRate * 2);
  u16(2);
  u16(16);
  ascii('data');
  u32(dataBytes);
  for (var i = 0; i < frames; i++) {
    final v = (0.3 * sin(2 * pi * freq * i / sampleRate) * 32767).round();
    view.setInt16(o, v, Endian.little);
    o += 2;
  }
  return bytes;
}
