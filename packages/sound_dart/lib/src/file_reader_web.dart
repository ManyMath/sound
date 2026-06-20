import 'dart:typed_data';

/// The web has no file system; reading a clip by path is unsupported there. Use
/// `SoundSource.bytes` (e.g. from an HTTP fetch or an asset) instead.
Future<Uint8List> readBytes(String path) async {
  throw UnsupportedError(
    'Reading a file by path is not supported on the web; pass the bytes via '
    'SoundSource.bytes instead.',
  );
}
