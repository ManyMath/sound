import 'dart:typed_data';
import 'dart:convert';
import 'dart:io';

import 'package:ed25519_edwards/ed25519_edwards.dart' as ed;
import 'package:hex/hex.dart';
import 'package:yaml/yaml.dart';

bool matchesSigningKey(String? privateHex, Object? publicHex) {
  if (privateHex == null ||
      !RegExp(r'^[0-9a-fA-F]{128}$').hasMatch(privateHex) ||
      publicHex is! String ||
      !RegExp(r'^[0-9a-fA-F]{64}$').hasMatch(publicHex)) {
    return false;
  }
  final bytes = HEX.decode(privateHex);
  final key = ed.newKeyFromSeed(Uint8List.fromList(bytes.sublist(0, 32)));
  if (HEX.encode(key.bytes) != privateHex.toLowerCase() ||
      HEX.encode(ed.public(key).bytes) != publicHex.toLowerCase()) {
    return false;
  }
  final probe = utf8.encode('ManyMath release signing preflight v1');
  return ed.verify(ed.public(key), probe, ed.sign(key, probe));
}

void main(List<String> args) {
  try {
    if (args.length != 1) throw StateError('Expected cargokit configuration');
    final config = loadYaml(File(args.single).readAsStringSync());
    if (!matchesSigningKey(
      Platform.environment['PRIVATE_KEY'],
      config['precompiled_binaries']['public_key'],
    )) {
      throw StateError('Signing key unavailable or mismatched');
    }
    stdout.writeln('Signing key matches committed verification key.');
  } catch (_) {
    // Never print the supplied key or exception input.
    stderr.writeln('Signing preflight failed; no release should be created.');
    exitCode = 1;
  }
}
