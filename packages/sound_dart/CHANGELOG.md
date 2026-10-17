# Changelog

## 0.0.1

- Initial scaffolding.
- Core API: `Sound`, `SoundSource`, `Playback`, pluggable `SoundBackend`s.
- FFI backend over the `sound_cli` Rust library: ALSA on Linux, AAudio on
  Android, CoreAudio on macOS/iOS, and waveOut on Windows. Web-safe via
  conditional imports.
- WebAudio backend for the browser (selected automatically on web).
- Native decoding of WAV/MP3/OGG-Vorbis/FLAC via the pure-Rust `symphonia`
  (no system dependencies); the browser decodes natively on web.
- Per-voice volume (`setVolume`, `volume:`) and looping (`setLooping`, `loop:`).
- Pause/resume, seek, and `position`/`duration` query.
- `Sound.playUrl`/`loadUrl` to fetch and play audio over HTTP (all platforms).
  Natively the stream starts playing before the download finishes, reporting
  `PlaybackState.connecting` until the first audio decodes.
- `Sound.openPcmStream` to pull a clip's decoded PCM frames as a stream
  without playing it.
- `SoundFade` extension: `fade`, `fadeIn`, `fadeOut` volume ramps.
- `Playback.volume` reports the current linear volume; `fade` and `fadeOut`
  start from it by default instead of `1.0`.
- Native decoding is bounded for hostile input: HTTP downloads over 256 MiB
  and audio decoding to more than 512 MiB of PCM fail with an error.
- `Sound.initialize()` initializes the active backend, falling back past an
  auto-selected one that fails (a pinned backend still throws).
- CLI example and a web example.
