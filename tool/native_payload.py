#!/usr/bin/env python3
"""Verify pub-selected archives in disposable workspaces; never publish.

Python 3.11+, pinned Dart/Flutter SDK, Cargo and platform build tools required.
Archive mode is deliberately a mandatory, explicit flag with no fallback.
"""
import argparse
import hashlib
import json
import os
import platform
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib


def run(args, cwd, log, env=None):
    # CreateProcess does not apply PATHEXT like a Windows shell. Resolve the
    # SDK's dart.bat/flutter.bat without relying on shell executable search.
    executable = shutil.which(args[0], path=(env or os.environ).get("PATH"))
    if executable is None:
        raise RuntimeError(f"Required executable not found: {args[0]}")
    result = subprocess.run([executable, *args[1:]], cwd=cwd, env=env,
                            encoding="utf-8", errors="replace", input="\n",
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    Path(log).write_text(result.stdout, encoding="utf-8")
    if result.returncode:
        raise RuntimeError(f"Command failed ({result.returncode}): {args}; see {log}")
    return result.stdout


def inside(path, root):
    path = path.resolve()
    if not path.is_relative_to(root.resolve()):
        raise ValueError(f"Path escapes package: {path}")
    return path


def extract(archive, destination):
    destination.mkdir(parents=True)
    with tarfile.open(archive) as source:
        for member in source.getmembers():
            inside(destination / member.name, destination)
            if not (member.isdir() or member.isfile()):
                raise ValueError(f"Unsupported archive member: {member.name}")
        source.extractall(destination, filter="data")


def inspect_package(root, spec):
    """Check required sources, Cargo path closure, and expected platform contracts."""
    for name in spec.get("required", []):
        if not inside(root / name, root).is_file():
            raise ValueError(f"Missing published source: {name}")
    for manifest in root.rglob("Cargo.toml"):
        data = tomllib.loads(manifest.read_text())
        def visit(value):
            if isinstance(value, dict):
                if "path" in value and isinstance(value["path"], str):
                    # Cargo dependency paths are directories; [lib]/[[bin]] paths
                    # are source files. Both must exist inside the package.
                    target = inside(manifest.parent / value["path"], root)
                    if not target.exists() or (target.is_dir() and not (target / "Cargo.toml").is_file()):
                        raise ValueError(f"Missing Cargo path: {manifest}: {value['path']}")
                for child in value.values():
                    visit(child)
            elif isinstance(value, list):
                for child in value:
                    visit(child)
        visit(data)
    for relative in spec.get("provenance", []):
        manifest = inside(root / relative, root)
        data = json.loads(manifest.read_text())
        for name, expected in data["files_sha256"].items():
            source = inside(manifest.parent / name, root)
            if hashlib.sha256(source.read_bytes()).hexdigest() != expected:
                raise ValueError(f"Stale staged source: {name}")
    for relative, fragment in spec.get("contracts", {}).items():
        path = inside(root / relative, root)
        if not path.is_file() or fragment not in path.read_text():
            raise ValueError(f"Missing platform contract in {relative}: {fragment}")


def snapshot(repo, destination):
    """Copy candidate files, including ignored files selected by .pubignore.

    Git's file list is not pub's file list: a package .pubignore can deliberately
    include generated source ignored by Git. Only Git administrative data is
    omitted; pub applies its own publication filtering to this complete copy.
    """
    def ignore_git(directory, names):
        return [".git"] if ".git" in names else []
    shutil.copytree(repo, destination, symlinks=True, ignore=ignore_git)
    subprocess.run(["git", "init", "--quiet", str(destination)], check=True)


def remove_workspace_resolution(package):
    # Hosted pub packages ignore workspace membership. Local path consumers
    # require this adjustment; the original archive remains untouched.
    manifest = package / "pubspec.yaml"
    manifest.write_text(re.sub(r"^resolution: workspace\s*\n", "", manifest.read_text(), flags=re.M))


def verify_candidate_constraints(output, packages):
    # Verify untouched published ranges before using source overrides.
    # Use Dart's own semver implementation, independent of workspace resolution.
    check = output / "candidate-constraints"
    check.mkdir()
    (check / "pubspec.yaml").write_text("name: candidate_constraints\npublish_to: none\nenvironment:\n  sdk: ^3.10.0\ndependencies:\n  pub_semver: ^2.2.0\n  yaml: ^3.1.3\n")
    (check / "check.dart").write_text(r'''import 'dart:io';
import 'package:pub_semver/pub_semver.dart';
import 'package:yaml/yaml.dart';
void main(List<String> args) {
  final docs = [for (final path in args) loadYaml(File(path).readAsStringSync()) as YamlMap];
  final versions = {for (final doc in docs) doc['name']: Version.parse(doc['version'] as String)};
  for (final doc in docs) {
    final deps = doc['dependencies'] as YamlMap?;
    if (deps == null) continue;
    for (final name in versions.keys) {
      if (!deps.containsKey(name)) continue;
      final dep = deps[name];
      final constraint = dep is String ? dep : (dep is YamlMap ? dep['version'] : null);
      if (constraint is! String || !VersionConstraint.parse(constraint).allows(versions[name]!)) {
        throw StateError('${doc['name']}: $name ${versions[name]} does not satisfy declared constraint $constraint');
      }
    }
  }
  print('All coordinated candidate versions satisfy published dependency constraints');
}
''')
    run(["dart", "pub", "get"], check, output / "candidate-constraints-get.log")
    run(["dart", "run", "check.dart"] + [str(output / "packages" / item["name"] / "pubspec.yaml") for item in packages], check, output / "candidate-constraints.log")


def required_rust_targets(config, target, machine=None):
    """Debug macOS builds select the host arch in the pinned Flutter SDK."""
    configured = config["rust_targets"].get(target)
    if not configured:
        raise RuntimeError(f"No provisioned Rust target contract for {target}")
    arch = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64", "amd64": "x86_64"}.get((machine or platform.machine()).lower())
    if any("{host_arch}" in value for value in configured) and arch is None:
        raise RuntimeError("Unsupported host architecture for native payload")
    return [value.replace("{host_arch}", arch or "") for value in configured]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--mode", choices=["hosted", "candidate"], required=True)
    parser.add_argument("--target", default="linux")
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    repo = args.config.resolve().parent.parent
    output = args.output.resolve()
    if output.exists():
        raise ValueError("Output must not exist: evidence from different runs must stay separate")
    output.mkdir(parents=True)
    version = run(["dart", "--version"], repo, output / "dart-version.log")
    if f"Dart SDK version: {config['dart_sdk']} " not in version:
        raise ValueError(f"Unsupported Dart SDK for hidden archive flag: {version.strip()}")
    if config.get("rust_toolchain"):
        channel = config["rust_toolchain"]
        installed = run(["rustup", "toolchain", "list"], repo, output / "rust-toolchains.log")
        if not any(line.split()[0] == channel or line.split()[0].startswith(channel + "-") for line in installed.splitlines()):
            raise RuntimeError(f"Provision Rust toolchain {channel} before verification; no installer fallback")
        targets = run(["rustup", "target", "list", "--installed", "--toolchain", channel], repo, output / "rust-targets.log").splitlines()
        required = required_rust_targets(config, args.target)
        if not required or not set(required).issubset(targets):
            raise RuntimeError(f"Provision Rust targets for {args.target} before verification; no installer fallback")
        run(["rustup", "run", channel, "rustc", "--version", "--verbose"], repo, output / "native-rust-version.log")
    report = {"mode": args.mode, "target": args.target, "dart": version.strip(),
              "source_revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo).decode().strip(),
              "source_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=repo)),
              "archives": {}, "status": "incomplete"}
    (output / "evidence.json").write_text(json.dumps(report, indent=2))
    with tempfile.TemporaryDirectory(prefix="native-payload-stage-") as temp:
        stage = Path(temp) / "source"
        snapshot(repo, stage)
        if config.get("prepare"):
            run(config["prepare"] + ["--staged-root", str(stage), "--revision", report["source_revision"]], stage, output / "prepare.log")
        run(["flutter", "pub", "get"], stage, output / "workspace-get.log")
        if config.get("analysis_command"):
            run(config["analysis_command"], stage, output / "source-analysis.log")
        for spec in config["packages"]:
            name = spec["name"]
            package = stage / spec["path"]
            archive = output / f"{name}.tar.gz"
            # Never combine --dry-run and --to-archive: some SDKs silently
            # succeed without writing anything. Never drop --to-archive.
            run(["dart", "pub", "publish", "--dry-run"], package, output / f"{name}-validation.log")
            run(["dart", "pub", "publish", f"--to-archive={archive}"], package, output / f"{name}-archive.log")
            if not archive.is_file() or not archive.stat().st_size:
                raise RuntimeError("Pinned SDK did not create archive; refusing fallback")
            extracted = output / "packages" / name
            extract(archive, extracted)
            inspect_package(extracted, spec)
            report["archives"][name] = {"sha256": hashlib.sha256(archive.read_bytes()).hexdigest()}
            remove_workspace_resolution(extracted)
    flutter_spec = config["flutter_package"]
    consumer = output / "flutter-consumer"
    run(["flutter", "create", "--no-pub", "--platforms=" + ("android" if args.target in {"apk", "appbundle"} else args.target), "--project-name=payload_consumer", str(consumer)], output, output / "flutter-create.log")
    # Consumer-local options override any ancestor user configuration. The gate
    # verifies source builds, never downloaded binaries or a matching local cache.
    (consumer / "cargokit_options.yaml").write_text(
        "use_precompiled_binaries: false\nuse_local_precompiled_binaries: false\ncache_local_builds: false\n")
    manifest = consumer / "pubspec.yaml"
    text = manifest.read_text().replace("dependencies:\n", f"dependencies:\n  {flutter_spec}:\n    path: ../packages/{flutter_spec}\n", 1)
    if args.mode == "candidate":
        verify_candidate_constraints(output, config["packages"])
        report["candidate_constraints"] = "passed (pub_semver checks original published constraints)"
        text += "\ndependency_overrides:\n"
        for item in config["packages"]:
            if item["name"] != flutter_spec:
                text += f"  {item['name']}:\n    path: ../packages/{item['name']}\n"

    manifest.write_text(text)
    (consumer / "lib/main.dart").write_text(config["flutter_main"])
    run(["flutter", "pub", "get"], consumer, output / "flutter-get.log")
    run(["flutter", "build", args.target, "--debug"] + (["--no-codesign"] if args.target == "ios" else []), consumer, output / "flutter-build.log")
    report["flutter_build"] = "passed"
    # Per-repository smoke program explicitly loads the native library and
    # tests minimal behavior without requiring a physical capture device.
    if args.target == "linux":
        matches = list((consumer / "build/linux").rglob(config["library_name"]))
        bundled = [p for p in matches if "/bundle/lib/" in str(p)]
        if len(bundled) != 1:
            raise RuntimeError(f"Expected exactly one bundled native library, found {bundled}")
        if config.get("flutter_runtime_command"):
            bundle = bundled[0].parent.parent
            if config.get("relocate_flutter_bundle", False):
                relocated = output / "runtime-bundle"
                shutil.copytree(bundle, relocated)
                bundle = relocated
            executable = bundle / "payload_consumer"
            command = [part.replace("{executable}", str(executable)) for part in config["flutter_runtime_command"]]
            runtime_env = dict(os.environ, **config.get("flutter_runtime_environment", {}))
            if config.get("library_environment"):
                runtime_env[config["library_environment"]] = str(bundle / "lib" / config["library_name"])
            run(command, executable.parent, output / "flutter-smoke.log", runtime_env)
            report["flutter_native_smoke"] = "passed (configured Flutter native behavior)"
        else:
            cli = output / "dart-consumer"
            cli.mkdir()
            dart_package = config["dart_package"]
            dep = (f"    path: ../packages/{dart_package}\n" if args.mode == "candidate"
                   else f"    {config['hosted_dart_constraint']}\n")
            # Scalar hosted constraints use the normal dependency form.
            dependency = (f"  {dart_package}:\n{dep}" if args.mode == "candidate"
                          else f"  {dart_package}: {config['hosted_dart_constraint']}\n")
            (cli / "pubspec.yaml").write_text("name: payload_dart_consumer\nenvironment:\n  sdk: ^3.10.0\ndependencies:\n" + dependency)
            (cli / "main.dart").write_text(config["dart_main"])
            run(["dart", "pub", "get"], cli, output / "dart-get.log")
            env = dict(os.environ, **{config["library_environment"]: str(bundled[0])})
            if config.get("copy_library_to_cli", False):
                copied = cli / config["library_name"]
                shutil.copy2(bundled[0], copied)
                env[config["library_environment"]] = str(copied)
            for source, destination in config.get("smoke_files", {}).items():
                shutil.copy2(inside(output / "packages" / flutter_spec / source, output / "packages" / flutter_spec), inside(cli / destination, cli))
            run(["dart", "run", "main.dart"], cli, output / "dart-smoke.log", env)
            report["dart_native_smoke"] = "passed (library loading and configured minimal behavior)"
    report["status"] = "passed"
    (output / "evidence.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
