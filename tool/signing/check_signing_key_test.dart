import 'dart:typed_data';

import 'package:ed25519_edwards/ed25519_edwards.dart' as ed;
import 'package:hex/hex.dart';

import 'check_signing_key.dart';

void main() {
  final key = ed.newKeyFromSeed(Uint8List.fromList(List<int>.filled(32, 7)));
  final secret = HEX.encode(key.bytes);
  final public = HEX.encode(ed.public(key).bytes);
  final cases = <bool>[
    matchesSigningKey(secret, public),
    !matchesSigningKey(null, public),
    !matchesSigningKey('', public),
    !matchesSigningKey('bad', public),
    !matchesSigningKey(secret, List.filled(64, '0').join()),
    !matchesSigningKey(secret, 1),
    !matchesSigningKey('00${secret.substring(2)}', public),
  ];
  for (var i = 0; i < cases.length; i++) {
    if (!cases[i]) throw StateError('Signing preflight regression $i');
  }
  print('${cases.length} signing preflight cases passed');
}
