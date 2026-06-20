import 'dart:typed_data';

import 'exceptions.dart';
import 'file_reader.dart';
import 'pcm.dart';
import 'sound_source.dart';

/// Decoded PCM from a clip: interleaved Int16 [samples] in [format].
class DecodedPcm {
  const DecodedPcm(this.format, this.samples);

  final PcmFormat format;
  final Int16List samples;
}

/// Decodes [source] to PCM and exposes it as a [PcmStream] of interleaved Int16
/// frames, without sending anything to an audio device.
///
/// This is the playback-path "tap": it lets a clip be pulled as data (to mix,
/// analyze, or re-route) rather than heard. 16-bit integer PCM WAV is decoded
/// in pure Dart here, so it works under any backend -- including
/// `SilentBackend` and on the web -- because it never opens the device.
/// Compressed formats (MP3/OGG/FLAC) are a roadmap item that will route through
/// the native sound_cli decoder.
///
/// [chunkFrames] sets how many frames each emitted Int16List carries. Throws
/// [UnsupportedSourceException] for a source this tap cannot decode.
Future<PcmStream> openPcmStream(
  SoundSource source, {
  int chunkFrames = 2048,
}) async {
  if (chunkFrames < 1) {
    throw ArgumentError.value(chunkFrames, 'chunkFrames', 'must be positive');
  }
  final bytes = await _bytesOf(source);
  final decoded = decodeWavPcm(bytes);
  return PcmStream(
    format: decoded.format,
    frames: _chunked(decoded.samples, decoded.format, chunkFrames),
  );
}

Future<Uint8List> _bytesOf(SoundSource source) => switch (source) {
  BytesSource(:final bytes) => Future.value(bytes),
  FileSource(:final path) => readBytes(path),
};

Stream<Int16List> _chunked(
  Int16List samples,
  PcmFormat format,
  int chunkFrames,
) async* {
  final step = chunkFrames * format.channels;
  for (var i = 0; i < samples.length; i += step) {
    final end = i + step < samples.length ? i + step : samples.length;
    yield samples.sublist(i, end);
  }
}

/// Decodes a 16-bit integer PCM WAV (RIFF) container in pure Dart.
///
/// Throws [UnsupportedSourceException] if the bytes are not a 16-bit PCM WAV.
DecodedPcm decodeWavPcm(Uint8List bytes) {
  final view = ByteData.sublistView(bytes);
  String tag(int o) => String.fromCharCodes(bytes, o, o + 4);

  if (bytes.length < 12 || tag(0) != 'RIFF' || tag(8) != 'WAVE') {
    throw const UnsupportedSourceException(
      'PCM tap supports 16-bit WAV; the bytes are not a RIFF/WAVE container.',
    );
  }

  int? audioFormat;
  int? channels;
  int? sampleRate;
  int? bits;
  var dataStart = -1;
  var dataLen = 0;

  var o = 12;
  while (o + 8 <= bytes.length) {
    final id = tag(o);
    final size = view.getUint32(o + 4, Endian.little);
    final body = o + 8;
    if (id == 'fmt ' && body + 16 <= bytes.length) {
      audioFormat = view.getUint16(body, Endian.little);
      channels = view.getUint16(body + 2, Endian.little);
      sampleRate = view.getUint32(body + 4, Endian.little);
      bits = view.getUint16(body + 14, Endian.little);
    } else if (id == 'data') {
      dataStart = body;
      dataLen = size;
    }
    // Chunks are word-aligned: an odd size carries a trailing pad byte.
    o = body + size + (size.isOdd ? 1 : 0);
  }

  if (channels == null || sampleRate == null || bits == null || dataStart < 0) {
    throw const UnsupportedSourceException(
      'PCM tap could not find the fmt and data chunks in the WAV container.',
    );
  }
  if (audioFormat != 1 || bits != 16) {
    throw UnsupportedSourceException(
      'PCM tap supports 16-bit integer PCM WAV only '
      '(found format $audioFormat, $bits-bit).',
    );
  }

  // Clamp to the bytes actually present, rounded to whole samples.
  final available = bytes.length - dataStart;
  var len = dataLen <= available ? dataLen : available;
  len -= len % 2;
  final samples = Int16List(len ~/ 2);
  for (var i = 0; i < samples.length; i++) {
    samples[i] = view.getInt16(dataStart + i * 2, Endian.little);
  }
  return DecodedPcm(
    PcmFormat(sampleRate: sampleRate, channels: channels),
    samples,
  );
}
