# Native payload verification

Run `python3 -m unittest discover -s tool/tests -v`, then
`python3 tool/native_payload.py --config tool/payload.json --mode candidate --output /tmp/sound-candidate`.
Use a new output directory for every run. Then use
`python3 tool/hosted_dart_smoke.py --candidate /tmp/sound-candidate --output /tmp/sound-hosted-dart`
to test published sound_dart 0.0.1 against the exact candidate native library,
without Flutter overrides. The candidate versions must satisfy their original
declared ranges.

Hosted Flutter resolution is deliberately blocked until the owner publishes
sound_dart 0.0.2 first: sound_flutter calls Sound.initialize(), absent from hosted
0.0.1, and now correctly requires ^0.0.2. The original ^0.0.1 constraint produced
an actual hosted-consumer compile failure. `--mode hosted` retains that full
Flutter gate (and a matching ^0.0.2 Dart consumer) for use after the Dart release;
no override may conceal it. The dedicated legacy script stays pinned to 0.0.1.

The workspace lockfile is aligned to the provisioned Flutter 3.47.2 / Dart 3.13.2
SDK; package minimum constraints remain independently declared.

The pinned Dart 3.13.2 SDK creates the actual pub-selected archives using its
explicit archive option, separately from validation. Missing archive support
fails closed. A complete disposable snapshot honors package .pubignore rules;
the original checkout and archive bytes are unchanged. Required source and
platform references are checked after extraction, then a fresh Flutter consumer
builds. Consumer-local Cargokit options disable downloaded/local prebuilts and
cached native builds so this verifies source compilation.

Linux smoke asserts the exact loaded library using /proc/self/maps and verifies
native player creation, an idle loaded WAV handle and disposal. `--target windows`
runs the same smoke against the bundled `runner/Debug/sound_cli.dll`: the process
module list must contain exactly that file, a same-named DLL on PATH is refused,
and a synthetic WAV must decode to exact PCM through the C ABI. No audio device is
opened. The evidence records the library path and SHA256, and
`hosted_dart_smoke.py` accepts that Windows candidate. The current
candidate decodes at load; hosted 0.0.1 delays decoding until playback. This is
not evidence of speaker output, hardware interruption handling, or microphone
mixing. Native and Dart regressions exercise those hardware-independent seams.

`--target macos` and `--target windows` require the preprovisioned host Rust target;
unknown targets or missing tools fail before any installer. On Windows the gate sets
RUSTUP_WINDOWS_PATH_ADD_BIN=1 for its children only: rustup proxies installed as
symlinks cannot be started by processes that enforce redirection trust (os error
448). `.gitattributes` keeps the hash-pinned Cargokit bytes LF under
core.autocrlf; other text files in such a checkout are archived with CRLF. Linux CI runs all
native tests, source analysis/tests, Web JS compile, example build, signing-key
fixtures and the actual-archive candidate plus legacy hosted Dart consumer. Android/iOS and Windows/macOS builds
remain explicit fleet verification requirements; the legacy GitHub workflow
retains its platform matrix and is never dispatched by maintenance.

`tool/consumer_build.sh` remains a git-export comparison smoke and is not a
publication-payload gate. The actual archive gate above is authoritative.

# Signing and publication

No package is published by this gate. GitHub precompile release workflows are
owner-only manual preparation, guarded by repository, GitHub server and dispatch
event. Signing-key derivation/challenge validation runs before release creation;
platform uploads depend on that gate. The existing trust key is retained.
Only the owner may mirror or invoke publication after platform verification and
key-custody checks. `dart --packages=tool/signing/.dart_tool/package_config.json
tool/signing/check_signing_key_test.dart` exercises synthetic keys without
accessing the production signing secret.
