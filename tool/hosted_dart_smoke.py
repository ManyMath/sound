#!/usr/bin/env python3
"""Check old hosted Dart against the new native payload, without Flutter overrides."""
import argparse
import hashlib
import json
import os
from pathlib import Path
from native_payload import bundled_library, run, runtime_library


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    candidate = args.candidate.resolve()
    config = json.loads((Path(__file__).parent / "payload.json").read_text())
    evidence = json.loads((candidate / "evidence.json").read_text())
    target = evidence.get("target")
    spec = runtime_library(config, target)
    if evidence.get("status") != "passed" or evidence.get("mode") != "candidate" or spec is None:
        raise RuntimeError("Requires a successful exact-archive candidate for a target with a runtime smoke")
    library = bundled_library(candidate / "flutter-consumer", target, spec).resolve()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    (output / "pubspec.yaml").write_text("name: hosted_sound_compatibility\nenvironment:\n  sdk: ^3.10.0\ndependencies:\n  sound_dart: 0.0.1\n")
    (output / "main.dart").write_text(config["dart_main"])
    run(["dart", "pub", "get"], output, output / "pub-get.log")
    run(["dart", "run", "main.dart"], output, output / "native-smoke.log", dict(os.environ, SOUND_DART_LIB=str(library)))
    result = {"status": "passed", "target": target, "scope":"Hosted sound_dart 0.0.1 with exact candidate native library; no Flutter dependency override", "source_revision": evidence["source_revision"], "source_dirty": evidence["source_dirty"], "native_sha256": hashlib.sha256(library.read_bytes()).hexdigest(), "candidate_archives": evidence["archives"]}
    (output / "evidence.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
