## 0.0.2

- Require sound_dart ^0.0.2 for the Sound.initialize API; publish Dart first.

- Prepare a source-complete native payload with exact pub archive consumer verification.
- Preserve canonical audio safety fixes while incorporating reviewed bounded backpressure and Windows device-name handling.

## 0.0.1

- Flutter integration for `sound_dart`: builds the shared `sound_cli` Rust
  crate via cargokit and bundles it. Re-exports the `sound_dart` API and adds
  `SoundFlutter.ensureInitialized()`. Builds, bundles and loads on Android,
  iOS, Linux, macOS and Windows.
- `SoundFlutter.loadAsset`/`playAsset` to play bundled Flutter assets (with an
  optional `package`/`bundle`).
- `SoundFlutter.ensureInitialized()` now goes through `Sound.initialize()`, so
  a failing auto-selected backend falls back (e.g. to `silent`) instead of
  throwing.
