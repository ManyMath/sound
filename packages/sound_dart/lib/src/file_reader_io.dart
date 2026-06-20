import 'dart:io';
import 'dart:typed_data';

/// Reads the bytes of the file at [path] using dart:io.
Future<Uint8List> readBytes(String path) => File(path).readAsBytes();
