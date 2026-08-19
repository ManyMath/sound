import 'dart:typed_data';

/// Describes audio to be played, independent of how a backend loads it.
///
/// This is a sealed hierarchy so backends can exhaustively switch over the
/// concrete kinds they support and surface a clear error for the rest.
sealed class SoundSource {
  const SoundSource();

  /// Audio held in memory (e.g. a decoded asset or downloaded file).
  ///
  /// [format] is an optional hint such as `wav`, `mp3` or `ogg`. When omitted,
  /// a backend may sniff the container from the bytes.
  const factory SoundSource.bytes(Uint8List bytes, {String? format}) =
      BytesSource;

  /// Audio read from a file on disk by [path].
  const factory SoundSource.file(String path) = FileSource;

  /// Audio streamed from `http://`/`https://` [uri], sent with [headers]
  /// (e.g. an `Authorization` bearer token). Playback starts once enough of
  /// the response has downloaded to know the stream's format, rather than
  /// waiting for the whole file — see the native `sound_cli` crate's
  /// `net`/`streaming_source`/`decode` modules for how a backend implements
  /// this. [format] is an optional hint such as `wav`, `mp3` or `ogg` for
  /// URLs whose path has no recognizable extension.
  const factory SoundSource.uri(
    Uri uri, {
    Map<String, String> headers,
    String? format,
  }) = UriSource;
}

/// In-memory audio. See [SoundSource.bytes].
final class BytesSource extends SoundSource {
  const BytesSource(this.bytes, {this.format});

  final Uint8List bytes;

  /// Optional container/format hint (e.g. `wav`).
  final String? format;
}

/// A file on disk. See [SoundSource.file].
final class FileSource extends SoundSource {
  const FileSource(this.path);

  final String path;
}

/// A streamed network resource. See [SoundSource.uri].
final class UriSource extends SoundSource {
  const UriSource(this.uri, {this.headers = const {}, this.format});

  final Uri uri;
  final Map<String, String> headers;

  /// Optional container/format hint (e.g. `wav`).
  final String? format;
}
