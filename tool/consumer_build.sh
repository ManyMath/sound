#!/usr/bin/env bash
# Usage: tool/consumer_build.sh <flutter build target>...   e.g. linux apk
#
# Simulates a pub.dev install of sound_flutter/sound_dart: exports only the two
# package directories at HEAD (no monorepo around them, like the pub cache),
# then builds a freshly created app that depends on them. Catches build files
# that reach outside the package or break on what `flutter create` ships today.
#
# Set KEEP=1 to keep the temp dir (it is always kept on failure).
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <flutter build target>..." >&2
  exit 2
fi

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d)"
cleanup() {
  status=$?
  if [ "${KEEP:-0}" = 1 ] || [ "$status" -ne 0 ]; then
    echo "work dir kept at: $work" >&2
  else
    rm -rf "$work"
  fi
}
trap cleanup EXIT

# Platforms to scaffold: one per requested target (apk/appbundle -> android).
platforms=""
for target in "$@"; do
  case "$target" in
    apk | appbundle) p=android ;;
    *) p="$target" ;;
  esac
  case ",$platforms," in
    *",$p,"*) ;;
    *) platforms="${platforms:+$platforms,}$p" ;;
  esac
done

mkdir -p "$work/export" "$work/pkgs"
git -C "$repo" archive HEAD packages/sound_flutter packages/sound_dart | tar -x -C "$work/export"
mv "$work/export/packages/sound_flutter" "$work/export/packages/sound_dart" "$work/pkgs/"
rm -rf "$work/export" "$work/pkgs/sound_flutter/example"
# Outside the workspace these are ordinary packages. (grep/mv rather than
# sed -i, whose flags differ between GNU and BSD.)
for f in "$work"/pkgs/*/pubspec.yaml; do
  grep -v '^resolution: workspace$' "$f" > "$f.tmp"
  mv "$f.tmp" "$f"
done

cd "$work"
flutter create --platforms="$platforms" --project-name consumer app >/dev/null
cd app
cat >> pubspec.yaml <<'YAML'

dependency_overrides:
  sound_dart:
    path: ../pkgs/sound_dart
YAML
awk '{print} /^dependencies:$/ {print "  sound_flutter:\n    path: ../pkgs/sound_flutter"}' \
  pubspec.yaml > pubspec.yaml.tmp
mv pubspec.yaml.tmp pubspec.yaml
cat > lib/main.dart <<'DART'
import 'package:flutter/material.dart';
import 'package:sound_flutter/sound_flutter.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  final backend = await SoundFlutter.ensureInitialized();
  runApp(MaterialApp(home: Text('backend: $backend')));
}
DART

flutter pub get
for target in "$@"; do
  echo "=== flutter build $target --debug"
  if [ "$target" = ios ]; then
    flutter build ios --debug --no-codesign
  else
    flutter build "$target" --debug
  fi
done
