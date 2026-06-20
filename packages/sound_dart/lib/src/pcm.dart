import 'dart:typed_data';

/// The shape of PCM produced by a [PcmStream]: interleaved signed 16-bit
/// samples at [sampleRate] across [channels].
class PcmFormat {
  const PcmFormat({required this.sampleRate, required this.channels})
    : assert(sampleRate > 0, 'sampleRate must be positive'),
      assert(channels > 0, 'channels must be positive');

  /// Frames per second (e.g. 44100, 48000).
  final int sampleRate;

  /// Channel count (1 mono, 2 stereo).
  final int channels;

  /// Bytes per frame: [channels] samples, each two bytes (16-bit).
  int get bytesPerFrame => channels * 2;

  PcmFormat copyWith({int? sampleRate, int? channels}) => PcmFormat(
    sampleRate: sampleRate ?? this.sampleRate,
    channels: channels ?? this.channels,
  );

  @override
  bool operator ==(Object other) =>
      other is PcmFormat &&
      other.sampleRate == sampleRate &&
      other.channels == channels;

  @override
  int get hashCode => Object.hash(sampleRate, channels);

  @override
  String toString() => 'PcmFormat($sampleRate Hz, $channels ch)';
}

/// Decoded PCM pulled from a clip: a stream of interleaved Int16 [frames] plus
/// the [format] they are in.
///
/// This is the output of the playback-path "tap" (see `Sound.openPcmStream`):
/// it lets a clip be consumed as data -- mixed, analyzed, or re-routed -- rather
/// than sent to an audio device.
class PcmStream {
  const PcmStream({required this.format, required this.frames});

  /// The format every [frames] event is in.
  final PcmFormat format;

  /// Interleaved signed 16-bit samples, chunked into frames.
  final Stream<Int16List> frames;
}
