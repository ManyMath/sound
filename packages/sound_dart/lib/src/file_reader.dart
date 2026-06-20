import 'dart:typed_data';

// Reads a file with dart:io on native platforms and throws on the web, where
// there is no file system. The conditional import keeps dart:io out of web
// builds entirely.
import 'file_reader_io.dart'
    if (dart.library.js_interop) 'file_reader_web.dart'
    as impl;

/// Reads the bytes of the file at [path]. Throws [UnsupportedError] on the web.
Future<Uint8List> readBytes(String path) => impl.readBytes(path);
