import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:sound_flutter/sound_flutter.dart';

/// An [AssetBundle] that returns fixed bytes for any key, so asset loading can
/// be tested without a built asset bundle.
class _FakeBundle extends CachingAssetBundle {
  _FakeBundle(this._data);
  final Uint8List _data;

  @override
  Future<ByteData> load(String key) async => ByteData.sublistView(_data);
}

/// A backend whose initialize() can fail, like a missing audio device.
class _FakeBackend extends SoundBackend {
  _FakeBackend(this.name, this.priority, {this.failInit = false});

  @override
  final String name;
  @override
  final int priority;
  final bool failInit;

  @override
  bool get isAvailable => true;

  @override
  Future<void> initialize() async {
    if (failInit) throw const PlaybackException('no audio device');
  }

  @override
  Future<Playback> load(
    SoundSource source, {
    double volume = 1.0,
    bool loop = false,
  }) async => SilentPlayback(Duration.zero);

  @override
  Future<void> dispose() async {}
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  setUp(() => Sound.reset());
  tearDown(() => Sound.reset());

  test('loadAsset reads bundle bytes and dispatches to the backend', () async {
    final silent = SilentBackend();
    Sound.registerBackend(silent, makeActive: true);

    final wav = Uint8List.fromList(List<int>.generate(1644, (i) => i % 256));
    final playback = await SoundFlutter.loadAsset(
      'assets/test_tone.wav',
      bundle: _FakeBundle(wav),
    );

    expect(playback, isNotNull);
    expect(silent.loaded, hasLength(1));
    final source = silent.loaded.single;
    expect(source, isA<BytesSource>());
    expect((source as BytesSource).bytes.length, 1644);
    expect(source.format, 'wav');
  });

  test('ensureInitialized returns the active backend name', () async {
    Sound.registerBackend(SilentBackend(), makeActive: true);
    expect(await SoundFlutter.ensureInitialized(), 'silent');
  });
  test('ensureInitialized skips an auto-selected backend that fails', () async {
    Sound.registerBackend(_FakeBackend('broken', 9000, failInit: true));
    Sound.registerBackend(_FakeBackend('usable', 8000));
    expect(await SoundFlutter.ensureInitialized(), 'usable');
  });

  test('ensureInitialized throws when the pinned backend fails', () async {
    Sound.registerBackend(
      _FakeBackend('broken', 9000, failInit: true),
      makeActive: true,
    );
    await expectLater(
      SoundFlutter.ensureInitialized(),
      throwsA(isA<PlaybackException>()),
    );
  });
}
