#!/usr/bin/env python3
"""Rebuild a vendored Cargokit tree offline from an exact commit and local patches.

Never fetches, changes a Git remote, installs SDKs, or overwrites a destination.
"""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import tempfile

PROVENANCE = 'VENDOR_PROVENANCE.json'


def digest(data):
    return hashlib.sha256(data).hexdigest()


def relative(value):
    path = PurePosixPath(value)
    if not value or path.is_absolute() or '..' in path.parts or '\\' in value or ':' in value or str(path) != value:
        raise ValueError(f'Unsafe relative path: {value}')
    if '.git' in path.parts:
        raise ValueError('Git administrative paths are forbidden')
    return path


def safe_file(root, name):
    path = root / relative(name)
    if not path.resolve().is_relative_to(root.resolve()) or path.is_symlink():
        raise ValueError(f'Path escapes tree or is a symlink: {name}')
    return path


def git(source, *args):
    return subprocess.check_output(['git', '-C', str(source), *args])


def load_recipe(path):
    recipe = json.loads(path.read_text())
    if recipe.get('schema') != 1 or not re.fullmatch(r'[0-9a-f]{40}', recipe.get('revision', '')):
        raise ValueError('Requires schema 1 and a complete immutable Git revision')
    if not recipe.get('files') or not recipe.get('expected_files_sha256'):
        raise ValueError('Explicit source and expected file inventories are required')
    for name in [*recipe['files'], *recipe['expected_files_sha256']]:
        relative(name)
    if PROVENANCE in recipe['expected_files_sha256']:
        raise ValueError('Generated provenance cannot hash itself')
    return recipe


def materialize(recipe_path, source, output):
    """Return a deterministic tree after verifying source, patches and final bytes."""
    recipe = load_recipe(recipe_path)
    revision = recipe['revision']
    actual = git(source, 'rev-parse', '--verify', revision + '^{commit}').decode().strip()
    if actual != revision:
        raise ValueError('Source revision does not resolve to the required commit')
    tree = {}
    for row in git(source, 'ls-tree', '-rz', revision).split(b'\0'):
        if row:
            meta, name = row.split(b'\t', 1)
            mode, kind, oid = meta.decode().split()
            tree[name.decode()] = (mode, kind, oid)
    if set(tree) != set(recipe['files']) | set(recipe['omitted_files']):
        raise ValueError('Export and omission inventories do not cover the exact upstream tree')
    if set(recipe['files']) & set(recipe['omitted_files']):
        raise ValueError('An upstream path cannot be both exported and omitted')
    for name, expected in recipe['files'].items():
        mode, kind, oid = tree[name]
        if kind != 'blob' or mode not in ('100644', '100755'):
            raise ValueError(f'Unsupported upstream file type: {name}')
        data = git(source, 'cat-file', 'blob', oid)
        if digest(data) != expected['sha256'] or mode != expected['mode']:
            raise ValueError(f'Upstream source identity mismatch: {name}')
        target = safe_file(output, name)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
        target.chmod(0o755 if mode == '100755' else 0o644)
    for patch in recipe.get('patches', []):
        patch_path = safe_file(recipe_path.parent, patch['file'])
        if digest(patch_path.read_bytes()) != patch['sha256']:
            raise ValueError(f'Local patch identity mismatch: {patch["file"]}')
        if not patch.get('description'):
            raise ValueError('Every local patch needs a disposition description')
        # A fresh administrative directory prevents git apply from discovering
        # a parent checkout. Git rejects patch paths outside this staging tree.
        subprocess.run(['git', 'init', '-q', str(output)], check=True)
        # Patch exact bytes. Otherwise core.autocrlf (the Git for Windows
        # default) or global attributes rewrite patched files with CRLF.
        (output / '.git' / 'info').mkdir(exist_ok=True)
        (output / '.git' / 'info' / 'attributes').write_bytes(b'* -text\n')
        subprocess.run(['git', '-C', str(output), 'apply', '--check', str(patch_path.resolve())], check=True)
        subprocess.run(['git', '-C', str(output), 'apply', str(patch_path.resolve())], check=True)
    if (output / '.git').exists():
        shutil.rmtree(output / '.git')
    # Generated files are hashed, so write LF bytes rather than platform text.
    (output / '.vendored-revision').write_bytes((revision + '\n').encode())
    actual_files = {}
    for path in output.rglob('*'):
        if path.is_symlink():
            raise ValueError('Patched vendored trees cannot contain symlinks')
        if path.is_file():
            actual_files[path.relative_to(output).as_posix()] = digest(path.read_bytes())
    if actual_files != recipe['expected_files_sha256']:
        raise ValueError('Patched tree differs from the reviewed final file inventory')
    provenance = {'schema': 1, 'repository': recipe['repository'], 'revision': revision,
                  'recipe_sha256': digest(recipe_path.read_bytes()),
                  'patches': recipe.get('patches', []),
                  'installed_files_sha256': actual_files,
                  # Pub excludes hidden metadata such as .gitignore/.github.
                  # Retain the complete installed identity separately while the
                  # archive gate checks every expected visible tooling file.
                  'files_sha256': {name: sha for name, sha in actual_files.items()
                                   if not any(part.startswith('.') for part in PurePosixPath(name).parts)},
                  'publication_omitted_hidden_files': sorted(name for name in actual_files
                                   if any(part.startswith('.') for part in PurePosixPath(name).parts)),
                  'omitted_files': recipe['omitted_files']}
    (output / PROVENANCE).write_bytes((json.dumps(provenance, indent=2, sort_keys=True) + '\n').encode())
    return provenance


def compare(expected, installed):
    """Check all managed bytes. Extra tracked/unignored files are rejected too."""
    if installed.is_symlink():
        raise ValueError('Installed root must not be a symlink')
    expected_names = {p.relative_to(expected).as_posix() for p in expected.rglob('*') if p.is_file()}
    for name in expected_names:
        target = safe_file(installed, name)
        if not target.is_file() or target.read_bytes() != (expected / name).read_bytes():
            raise ValueError(f'Installed Cargokit drift: {name}')
        # Git records executable vs non-executable, not group/other access.
        # A runner umask of077 legitimately checks out100755 as0700.
        if bool(target.stat().st_mode & 0o100) != bool((expected / name).stat().st_mode & 0o100):
            # Windows does not preserve POSIX executable modes.
            import os
            if os.name != 'nt':
                raise ValueError(f'Installed executable mode drift: {name}')
    # Known generated files are ignored according to the owning Git checkout;
    # no ignored build/cache directory is ever modified by this verifier.
    listed = git(installed, 'ls-files', '--cached', '--others', '--exclude-standard', '-z', '--', '.')
    extra = set(p.decode() for p in listed.split(b'\0') if p) - expected_names
    if extra:
        raise ValueError(f'Unexpected managed files: {sorted(extra)}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', required=True, type=Path)
    parser.add_argument('--source', required=True, type=Path, help='Local Cargokit Git clone; no network access')
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument('--output', type=Path, help='New directory for review/install; must not exist')
    action.add_argument('--check', type=Path, help='Verify the installed vendored tree without writing it')
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='cargokit-sync-') as temporary:
        staged = Path(temporary) / 'tree'
        staged.mkdir()
        provenance = materialize(args.manifest.resolve(), args.source.resolve(), staged)
        if args.check:
            compare(staged, args.check.absolute())
        else:
            if args.output.exists():
                raise ValueError('Output already exists; refusing overwrite')
            shutil.copytree(staged, args.output)
    print(json.dumps({'status': 'verified' if args.check else 'staged',
                      'revision': provenance['revision'], 'recipe_sha256': provenance['recipe_sha256'],
                      'files': len(provenance['installed_files_sha256']), 'patches': len(provenance['patches'])}, indent=2))


if __name__ == '__main__':
    main()
