import 'dart:async';
import 'dart:typed_data';

import 'package:sound_dart/sound_dart.dart';
import 'package:test/test.dart';

/// A backend whose availability and priority are configurable, for exercising
/// selection logic.
class _ProbeBackend extends SoundBackend {
  _ProbeBackend(
    this.name, {
    this.available = true,
    this.priority = 0,
    this.failInit = false,
  });

  @override
  final String name;
  final bool available;
  final bool failInit;
  @override
  final int priority;

  int initializeCalls = 0;

  @override
  bool get isAvailable => available;

  @override
  Future<void> initialize() async {
    initializeCalls++;
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
  tearDown(() => Sound.reset());

  group('backend selection', () {
    // Probe priorities are kept well above the real defaults (silent at -1000,
    // the FFI backend at 100) so these tests are deterministic regardless of
    // whether the native library happens to be built on the dev host.
    test('always registers the silent fallback', () {
      expect(Sound.backends.map((b) => b.name), contains('silent'));
      expect(Sound.backend.isAvailable, isTrue);
    });

    test('prefers the highest-priority available backend', () {
      Sound.registerBackend(_ProbeBackend('low', priority: 5000));
      Sound.registerBackend(_ProbeBackend('high', priority: 9000));
      expect(Sound.backend.name, 'high');
    });

    test('skips unavailable backends', () {
      Sound.registerBackend(
        _ProbeBackend('unavailable', priority: 9999, available: false),
      );
      Sound.registerBackend(_ProbeBackend('usable', priority: 9000));
      expect(Sound.backend.name, 'usable');
    });

    test('re-selects when a higher-priority backend is registered later', () {
      final first = Sound.backend.name;
      Sound.registerBackend(_ProbeBackend('better', priority: 9000));
      expect(Sound.backend.name, 'better');
      expect(first, isNot('better'));
    });

    test('makeActive pins a backend regardless of priority', () {
      Sound.registerBackend(_ProbeBackend('high', priority: 9000));
      final low = _ProbeBackend('low', priority: 1);
      Sound.registerBackend(low, makeActive: true);
      expect(Sound.backend.name, 'low');
    });

    test('re-registering a pinned backend keeps it pinned', () {
      Sound.registerBackend(_ProbeBackend('high', priority: 9000));
      final silent = SilentBackend();
      Sound.registerBackend(silent, makeActive: true);
      Sound.registerBackend(silent);
      expect(Sound.backend, same(silent));
    });

    test('a pin survives later registrations', () {
      Sound.useBackend('silent');
      Sound.registerBackend(_ProbeBackend('better', priority: 9000));
      expect(Sound.backend.name, 'silent');
    });

    test('useBackend pins by name and throws on unknown', () {
      Sound.registerBackend(_ProbeBackend('a', priority: 1));
      Sound.registerBackend(_ProbeBackend('b', priority: 2));
      Sound.useBackend('a');
      expect(Sound.backend.name, 'a');
      expect(
        () => Sound.useBackend('nope'),
        throwsA(isA<NoBackendAvailableException>()),
      );
    });
  });

  group('playback', () {
    test('play loads and starts via the active backend', () async {
      final silent = SilentBackend();
      Sound.registerBackend(silent, makeActive: true);
      final playback = await Sound.playBytes(
        Uint8List.fromList([1, 2, 3]),
        format: 'wav',
      );
      expect(silent.loaded, hasLength(1));
      expect(silent.loaded.single, isA<BytesSource>());
      await playback.onComplete;
      expect(playback.state, PlaybackState.completed);
    });

    test('initialize is called once per backend', () async {
      final probe = _ProbeBackend('probe', priority: 10);
      Sound.registerBackend(probe, makeActive: true);
      await Sound.playBytes(Uint8List(0));
      await Sound.playBytes(Uint8List(0));
      expect(probe.initializeCalls, 1);
    });
  });

  group('initialization failure', () {
    test('skips an auto-selected backend that fails to initialize', () async {
      final broken = _ProbeBackend('broken', priority: 9000, failInit: true);
      Sound.registerBackend(broken);
      Sound.registerBackend(_ProbeBackend('usable', priority: 8000));
      await Sound.playBytes(Uint8List(0));
      expect(Sound.backend.name, 'usable');
      await Sound.playBytes(Uint8List(0));
      expect(broken.initializeCalls, 1);
    });

    test('rethrows when a pinned backend fails to initialize', () async {
      Sound.registerBackend(
        _ProbeBackend('broken', priority: 1, failInit: true),
        makeActive: true,
      );
      await expectLater(
        Sound.playBytes(Uint8List(0)),
        throwsA(isA<PlaybackException>()),
      );
      expect(Sound.backend.name, 'broken');
    });
  });

  group('Sound.initialize', () {
    test('skips an auto-selected backend that fails to initialize', () async {
      final broken = _ProbeBackend('broken', priority: 9000, failInit: true);
      Sound.registerBackend(broken);
      Sound.registerBackend(_ProbeBackend('usable', priority: 8000));
      expect((await Sound.initialize()).name, 'usable');
      expect(broken.initializeCalls, 1);
    });

    test('rethrows when the pinned backend fails to initialize', () async {
      Sound.registerBackend(
        _ProbeBackend('broken', priority: 1, failInit: true),
        makeActive: true,
      );
      await expectLater(Sound.initialize(), throwsA(isA<PlaybackException>()));
    });
  });

  group('registry robustness', () {
    test('registering the same instance twice is idempotent', () {
      final probe = _ProbeBackend('probe', priority: 9000);
      Sound.registerBackend(probe);
      Sound.registerBackend(probe);
      expect(Sound.backends.where((b) => b.name == 'probe'), hasLength(1));
    });

    test('reset clears the registry and re-seeds defaults', () async {
      Sound.registerBackend(_ProbeBackend('probe', priority: 9000));
      expect(Sound.backend.name, 'probe');
      await Sound.reset();
      // Defaults are re-seeded; the silent fallback is present again.
      expect(Sound.backends.map((b) => b.name), contains('silent'));
      expect(Sound.backends.map((b) => b.name), isNot(contains('probe')));
    });
  });

  group('SilentPlayback', () {
    test('volume tracks setVolume and clamps to silence', () async {
      final pb = SilentPlayback(Duration.zero, volume: 0.3);
      expect(pb.volume, 0.3);
      await pb.setVolume(1.5);
      expect(pb.volume, 1.5);
      await pb.setVolume(-1);
      expect(pb.volume, 0.0);
      await pb.setVolume(double.nan);
      expect(pb.volume, 0.0);
    });

    test('stop before completion marks stopped', () async {
      final pb = SilentPlayback(const Duration(seconds: 10));
      await pb.play();
      expect(pb.isPlaying, isTrue);
      await pb.stop();
      expect(pb.state, PlaybackState.stopped);
    });

    test('dispose completes onComplete and blocks further play', () async {
      final pb = SilentPlayback(const Duration(seconds: 10));
      await pb.dispose();
      await pb.onComplete; // should not hang
      await pb.play();
      expect(pb.state, PlaybackState.disposed);
    });

    test('stop when idle and double dispose are safe', () async {
      final pb = SilentPlayback(const Duration(seconds: 10));
      await pb.stop(); // never played
      expect(pb.state, isNot(PlaybackState.disposed));
      await pb.dispose();
      await pb.dispose(); // idempotent
      expect(pb.state, PlaybackState.disposed);
    });

    test('replaying re-arms onComplete for the new run', () async {
      final pb = SilentPlayback(const Duration(milliseconds: 30));
      await pb.play();
      await pb.onComplete;
      await pb.play();
      var done = false;
      unawaited(pb.onComplete.then((_) => done = true));
      await Future<void>.delayed(Duration.zero);
      expect(done, isFalse);
      await pb.onComplete.timeout(const Duration(seconds: 1));
      expect(pb.state, PlaybackState.completed);
    });

    test('stop while paused ends the run; resume does not revive it', () async {
      final pb = SilentPlayback(const Duration(seconds: 10));
      await pb.play();
      await pb.pause();
      await pb.stop();
      expect(pb.state, PlaybackState.stopped);
      await pb.resume();
      expect(pb.state, PlaybackState.stopped);
    });

    test('pause/resume after completion leave it completed', () async {
      final pb = SilentPlayback(Duration.zero);
      await pb.play();
      await pb.pause();
      await pb.resume();
      expect(pb.state, PlaybackState.completed);
    });

    test('pause/resume drive the paused state', () async {
      final pb = SilentPlayback(const Duration(seconds: 10));
      await pb.play();
      await pb.pause();
      expect(pb.state, PlaybackState.paused);
      await pb.resume();
      expect(pb.isPlaying, isTrue);
    });
  });
}
