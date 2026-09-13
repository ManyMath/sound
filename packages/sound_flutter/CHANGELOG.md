## 0.0.1

- Flutter integration for `sound_dart`: builds the shared `sound_cli` Rust
  crate via cargokit and bundles it. Re-exports the `sound_dart` API and adds
  `SoundFlutter.ensureInitialized()`. Builds, bundles and loads on Android,
  iOS, Linux, macOS and Windows.
- `SoundFlutter.loadAsset`/`playAsset` to play bundled Flutter assets (with an
  optional `package`/`bundle`).
