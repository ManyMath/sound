# Pinned Cargokit export

`manifest.json` records the exact private ManyMath fork revision, every exported
upstream byte/mode, every omitted upstream path, and ordered local patches with
SHA256 and rationale. The recipe's final SHA256 inventory binds the patch result.
`VENDOR_PROVENANCE.json` travels inside the package tooling;
no private repository import or submodule is needed by package consumers.
The complete installed identity includes hidden Git metadata and `.vendored-revision`;
pub omits those hidden paths. The provenance separately records that omission and
the visible published-file hashes checked by the actual archive gate.

The original copies omit six upstream lockfiles; this recipe records that policy
explicitly. Direct build_tool dependencies are pinned in its pubspec. This is not
an offline transitive dependency guarantee; release rehearsal must record and
pre-provision the actual resolved dependency closure separately.

Use a local clone of the private Cargokit fork (no network is accessed by the tool):

```
python3 tool/cargokit_sync.py --manifest tool/cargokit/manifest.json --source ../cargokit-sync --check packages/sound_flutter/cargokit
python3 tool/cargokit_sync.py --manifest tool/cargokit/manifest.json --source ../cargokit-sync --output /tmp/sound-cargokit-next
```

The output path must not exist. Verification rebuilds the tree independently,
checks bytes and executable bits, and rejects extra tracked/unignored source.
Known ignored build caches are left alone. The tool never overwrites an installed
tree or deletes local changes. Source and patch identity failures stop before any
review artifact is copied to the requested output.

To update: first run `--check` with the OLD recipe and retain its successful
report. Review a new immutable upstream commit, explicitly update export/omission
inventories and SHA256s, rebase each declared patch, and review the changed final
inventory. Generate a NEW output directory; compare it with the installed tree,
then copy only the reviewed managed files and remove only reviewed obsolete files
in a clean maintenance checkout. Never copy generated cache directories. Run the
fixture suite, `--check` with the new recipe, actual pub archive consumers, and
relevant platform CI before a guarded private commit/push. GitHub mirroring and
publication remain owner-only steps. Do not drop a local patch merely because an
upstream update makes it fail to apply.

The source is unchanged by this initial provenance rollout. The three Sound
patch groups preserve existing Windows symlink/PowerShell support, Gradle9
ExecOperations, and suppression of the Apple environment dump. The independent
native payload gate forces source compilation regardless of ancestor user cache
or prebuilt preferences.
