import 'dart:io';
import 'dart:typed_data';

import 'package:sound_dart/sound_dart.dart';
import 'package:test/test.dart';

/// Builds a 16-bit PCM WAV from [samples] (interleaved).
Uint8List _wav(List<int> samples, {int sampleRate = 8000, int channels = 1}) {
  final pcm = Int16List.fromList(samples);
  final data = pcm.buffer.asUint8List(pcm.offsetInBytes, pcm.lengthInBytes);
  final out = Uint8List(44 + data.length);
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
  u32(36 + data.length);
  ascii('WAVE');
  ascii('fmt ');
  u32(16);
  u16(1); // PCM
  u16(channels);
  u32(sampleRate);
  u32(sampleRate * channels * 2);
  u16(channels * 2);
  u16(16); // bits per sample
  ascii('data');
  u32(data.length);
  out.setRange(44, out.length, data);
  return out;
}

Future<List<int>> _collect(Stream<Int16List> frames) async {
  final out = <int>[];
  await for (final f in frames) {
    out.addAll(f);
  }
  return out;
}

void main() {
  tearDown(() => Sound.reset());

  test('decodes 16-bit PCM WAV bytes under SilentBackend', () async {
    Sound.registerBackend(SilentBackend(), makeActive: true);
    final pcm = await Sound.openPcmStream(
      SoundSource.bytes(_wav([1, -2, 3, -4], sampleRate: 8000, channels: 1)),
    );
    expect(pcm.format, const PcmFormat(sampleRate: 8000, channels: 1));
    expect(await _collect(pcm.frames), [1, -2, 3, -4]);
  });

  test('reports stereo format and preserves interleaving', () async {
    final pcm = await Sound.openPcmStream(
      SoundSource.bytes(_wav([1, 2, 3, 4], sampleRate: 44100, channels: 2)),
    );
    expect(pcm.format, const PcmFormat(sampleRate: 44100, channels: 2));
    expect(await _collect(pcm.frames), [1, 2, 3, 4]);
  });

  test('chunks frames by chunkFrames', () async {
    final pcm = await Sound.openPcmStream(
      SoundSource.bytes(_wav([10, 11, 12, 13, 14])),
      chunkFrames: 2,
    );
    final lengths = <int>[];
    await for (final f in pcm.frames) {
      lengths.add(f.length);
    }
    expect(lengths, [2, 2, 1]);
  });

  test('decodes from a file path', () async {
    final dir = await Directory.systemTemp.createTemp('sound_pcm_tap');
    try {
      final path = '${dir.path}/clip.wav';
      await File(path).writeAsBytes(_wav([7, 8, 9]));
      final pcm = await Sound.openPcmStream(SoundSource.file(path));
      expect(await _collect(pcm.frames), [7, 8, 9]);
    } finally {
      await dir.delete(recursive: true);
    }
  });

  test('rejects non-WAV bytes', () async {
    await expectLater(
      Sound.openPcmStream(
        SoundSource.bytes(Uint8List.fromList([0, 1, 2, 3, 4, 5, 6, 7])),
      ),
      throwsA(isA<UnsupportedSourceException>()),
    );
  });

  test('rejects non-16-bit PCM', () async {
    final wav = _wav([1, 2]);
    // Patch bits-per-sample (offset 34) to 8 to force an unsupported depth.
    ByteData.sublistView(wav).setUint16(34, 8, Endian.little);
    await expectLater(
      Sound.openPcmStream(SoundSource.bytes(wav)),
      throwsA(isA<UnsupportedSourceException>()),
    );
  });
}
