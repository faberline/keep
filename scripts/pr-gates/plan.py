#!/usr/bin/env python3
"""Manifest-derived unit/static plans. Does not grant PR qualification."""
import argparse
import glob
import json
import re
import sys
from pathlib import Path
import subprocess
import tomllib

LIB_KINDS = {'lib', 'rlib', 'dylib', 'cdylib', 'staticlib', 'proc-macro'}


def inventory(root, metadata):
    """Reject partial metadata and preserve every actual Cargo lib/bin target."""
    root = Path(root).resolve()
    manifest = tomllib.loads((root / 'Cargo.toml').read_text())
    workspace = manifest['workspace']
    # Cargo treats exclude entries as paths, not member-style glob patterns.
    excludes = {(root / pattern).resolve() for pattern in workspace.get('exclude', [])}
    def excluded(directory):
        return any(directory == path or directory.is_relative_to(path) for path in excludes)
    expected = set()
    for pattern in workspace.get('members', []):
        candidates = glob.glob(str(root / pattern), recursive=True, include_hidden=True)
        if not glob.has_magic(pattern) and not candidates:
            raise ValueError('partial workspace inventory: declared member missing')
        for candidate in candidates:
            directory = Path(candidate).resolve()
            member_manifest = directory / 'Cargo.toml'
            if not member_manifest.is_file():
                if not glob.has_magic(pattern):
                    raise ValueError('partial workspace inventory: declared member missing')
                continue
            # Cargo gives an explicitly named member precedence over exclude;
            # exclude still filters automatically expanded glob matches.
            if not glob.has_magic(pattern) or not excluded(directory):
                expected.add(member_manifest)
    if 'package' in manifest:
        expected.add(root / 'Cargo.toml')
    member_ids = metadata['workspace_members']
    members = set(member_ids)
    packages = [p for p in metadata['packages'] if p['id'] in members]
    by_manifest = {Path(p['manifest_path']).resolve(): p for p in packages}
    if not expected or not expected <= set(by_manifest):
        raise ValueError('partial workspace inventory: metadata and Cargo.toml disagree')
    if (len(member_ids) != len(members) or len(packages) != len(members)
            or len(by_manifest) != len(packages) or {p['id'] for p in packages} != members
            or len({p['name'] for p in packages}) != len(packages)):
        raise ValueError('missing or duplicate workspace package identity')
    # Cargo automatically includes in-workspace path dependencies, even if
    # the manifest does not list them in workspace.members. Prove every extra
    # member is reachable from an explicit member or root package, and refuse
    # metadata that omits a reachable, non-excluded local dependency.
    reachable = set(expected)
    pending = list(expected)
    while pending:
        source = by_manifest[pending.pop()]
        for dependency in source['dependencies']:
            if not dependency.get('path'):
                continue
            directory = Path(dependency['path']).resolve()
            if not directory.is_relative_to(root) or excluded(directory):
                continue
            target = directory / 'Cargo.toml'
            if not target.is_file():
                raise ValueError('partial workspace inventory: local dependency manifest missing')
            if target not in by_manifest:
                raise ValueError('partial workspace inventory: implicit path member missing')
            if dependency.get('name') != by_manifest[target]['name']:
                raise ValueError('workspace path dependency identity mismatch')
            if target not in reachable:
                reachable.add(target)
                pending.append(target)
    if reachable != set(by_manifest):
        raise ValueError('unexplained workspace member outside declared and path dependency closure')
    result = []
    for package in sorted(packages, key=lambda p: p['name']):
        targets = [t for t in package['targets'] if set(t['kind']) & (LIB_KINDS | {'bin'})]
        if not targets or any(not t.get('test', True) for t in targets):
            raise ValueError(f"{package['name']}: missing or disabled library/binary unit harness")
        package_manifest = tomllib.loads(Path(package['manifest_path']).read_text())
        for declaration in [package_manifest.get('lib', {}), *package_manifest.get('bin', [])]:
            if declaration.get('harness', True) is False or declaration.get('test', True) is False:
                raise ValueError(f"{package['name']}: disabled library/binary unit harness in manifest")
        result.append({
            'name': package['name'],
            'directory': str(Path(package['manifest_path']).resolve().parent.relative_to(root)),
            'features': package['features'],
            'targets': [{'name': t['name'], 'kind': t['kind'],
                         'required-features': t.get('required-features', []),
                         'harness': True, 'test': True} for t in targets],
            'dependencies': sorted({d['name'] for d in package['dependencies']}),
            'optional_dependencies': sorted({d.get('rename') or d['name']
                                             for d in package['dependencies'] if d.get('optional') is True}),
        })
        row = result[-1]
        coverage = [target_coverage(row, profile) for profile in profiles(row)]
        if any(not any(active for _, active in profile_targets) for profile_targets in coverage):
            raise ValueError(f"{package['name']}: profile has zero enabled unit targets; explicit reviewed matrix required")
        if any(not any(profile_targets[index][1] for profile_targets in coverage)
               for index in range(len(row['targets']))):
            raise ValueError(f"{package['name']}: unreachable library/binary unit target requirements")
    return result


def profiles(package):
    names = sorted(set(package['features']) - {'default'})
    return ['default'] + (['no-default'] + ['feature:' + n for n in names] + ['all-features'] if package['features'] else [])


def matrix(packages):
    rows = [{'package': p['name'], 'profile': profile} for p in packages for profile in profiles(p)]
    if len(rows) > 256:
        raise ValueError('matrix exceeds GitHub limit; split explicitly without dropping rows')
    # Get a result from every package early, then run every additional feature row.
    rows.sort(key=lambda row: (row['profile'] != 'default', row['package'], row['profile']))
    return {'include': rows}


def feature_args(profile):
    if profile == 'default':
        return []
    if profile == 'no-default':
        return ['--no-default-features']
    if profile == 'all-features':
        return ['--all-features']
    if profile.startswith('feature:') and profile[8:]:
        return ['--no-default-features', '--features', profile[8:]]
    raise ValueError('unknown feature profile')


def enabled_features(package, profile):
    roots = (set(package['features']) if profile == 'all-features' else
             {'default'} if profile == 'default' else
             {profile[8:]} if profile.startswith('feature:') else set())
    result = set()
    while roots - result:
        feature = sorted(roots - result)[0]
        result.add(feature)
        for value in package['features'].get(feature, []):
            if value in package['features']:
                roots.add(value)
            elif '/' in value:
                dependency, _ = value.split('/', 1)
                # Cargo enables an optional dependency for dep/feature, but
                # dep?/feature leaves it disabled. A matching crate feature
                # can then satisfy a target's required-features.
                if (not dependency.endswith('?') and dependency in package.get('optional_dependencies', [])
                        and dependency in package['features']):
                    roots.add(dependency)
    return result


def target_coverage(package, profile):
    enabled = enabled_features(package, profile)
    return [(target, set(target['required-features']) <= enabled)
            for target in package['targets']]


def command(package, profile, kind, target=None):
    if profile not in profiles(package):
        raise ValueError('profile is not in manifest-derived matrix')
    selectors = []
    for candidate, active in target_coverage(package, profile):
        if not active or (target is not None and target_key(candidate) != target_key(target)):
            continue
        if set(candidate['kind']) & LIB_KINDS:
            selectors.append('--lib')
        elif 'bin' in candidate['kind']:
            selectors.extend(['--bin', candidate['name']])
    if not selectors:
        raise ValueError('profile has zero enabled unit targets; explicit reviewed matrix required')
    if kind == 'unit':
        return ['cargo', 'test', '--locked', '-p', package['name'], *selectors, *feature_args(profile)]
    if kind == 'clippy':
        return ['cargo', 'clippy', '--locked', '-p', package['name'], '--all-targets', '--message-format=json',
                *feature_args(profile), '--', '-D', 'warnings']
    raise ValueError('unknown command kind')


def affected(packages, paths):
    """Advisory reverse-dependency closure; execution always uses full inventory."""
    names = {p['name'] for p in packages}
    changed = set()
    for path in paths:
        owners = {p['name'] for p in packages if path.startswith(p['directory'] + '/')}
        if not owners:  # docs, root config, deleted crate and unknown paths
            return sorted(names)
        changed |= owners
    if not paths:
        return sorted(names)
    while True:
        expanded = changed | {p['name'] for p in packages if changed & set(p['dependencies'])}
        if expanded == changed:
            return sorted(changed)
        changed = expanded


def load(root, metadata_json=None):
    if metadata_json is not None:
        return inventory(root, json.loads(Path(metadata_json).read_text()))
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1'],
        cwd=root, text=True))
    return inventory(root, metadata)


def target_key(target):
    return target['name'], tuple(sorted(target['kind']))


def compare_inventory(baseline, head):
    """No removal or loss of a previously enabled harness is self-approved."""
    old = {p['name']: p for p in baseline}
    new = {p['name']: p for p in head}
    removed = sorted(set(old) - set(new))
    if removed:
        raise ValueError(f'baseline workspace package removal requires review: {removed}')
    changes = []
    for name, before in old.items():
        after = new[name]
        lost_features = sorted(set(before['features']) - set(after['features']))
        if lost_features:
            raise ValueError(f'{name}: baseline feature removal requires review: {lost_features}')
        new_targets = {target_key(t): t for t in after['targets']}
        for target in before['targets']:
            key = target_key(target)
            if key not in new_targets:
                raise ValueError(f'{name}: baseline unit target removal requires review: {key}')
            current = new_targets[key]
            if not current.get('harness', True) or not current.get('test', True):
                raise ValueError(f'{name}: formerly enabled unit harness disabled: {key}')
            for profile in profiles(before):
                if (set(target['required-features']) <= enabled_features(before, profile)
                        and not set(current['required-features']) <= enabled_features(after, profile)):
                    raise ValueError(f'{name}: formerly enabled unit target lost in {profile}: {key}')
        if before != after:
            changes.append({'package': name, 'before': before, 'after': after})
    return {'added_packages': sorted(set(new) - set(old)), 'changed_packages': changes,
            'execution_selection': 'complete workspace', 'qualification': 'not evaluated'}


def verify_clean(root):
    """Reject visible changes to a checkout used as CI evidence."""
    status = subprocess.check_output(
        ['git', '-C', str(root), 'status', '--porcelain=v1', '--untracked-files=all',
         '--ignore-submodules=none'], text=True)
    if status:
        raise ValueError(f'checkout contains tracked or untracked changes: {root}')


def verify_identity(root, expected_head, expected_base, baseline_root):
    """Resolve checkout identity through read-only argv calls, never shell text."""
    if not all((expected_head, expected_base, baseline_root)):
        raise ValueError('complete --expected-head --expected-base --baseline-root identity required')
    if not all(re.fullmatch(r'[0-9a-fA-F]{40}', sha) for sha in (expected_head, expected_base)):
        raise ValueError('expected identities must be full 40-hex commit SHAs')
    actual_head = subprocess.check_output(
        ['git', '-C', str(root), 'rev-parse', '--verify', 'HEAD'], text=True).strip()
    actual_base = subprocess.check_output(
        ['git', '-C', str(baseline_root), 'rev-parse', '--verify', 'HEAD'], text=True).strip()
    if actual_head.lower() != expected_head.lower():
        raise ValueError(f'stale head: expected {expected_head}, actual {actual_head}')
    if actual_base.lower() != expected_base.lower():
        raise ValueError(f'wrong baseline: expected {expected_base}, actual {actual_base}')
    verify_clean(root)
    verify_clean(baseline_root)
    return {'expected_head': expected_head, 'expected_base': expected_base,
            'actual_head': actual_head, 'actual_base': actual_base}


RUNNING = re.compile(r'^running (\d+) tests?$')
SUMMARY = re.compile(r'^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; '
                     r'(\d+) ignored; (\d+) measured; (\d+) filtered out;')
TEST_ROW = re.compile(r'^test (.+) \.\.\. (.+)$')
LIST_ROW = re.compile(r'^(.+): test$')
LIST_SUMMARY = re.compile(r'^(\d+) tests?, (\d+) benchmarks?$')


def _libtest_blocks(stdout, expected_targets, pending_ignored=False):
    """Parse complete rosters. Pending ignored names are never pass evidence."""
    if expected_targets <= 0:
        raise ValueError('zero expected targets requires reviewed applicability')
    blocks = []
    current = None
    for line in stdout.splitlines():
        running = RUNNING.fullmatch(line)
        summary = SUMMARY.match(line)
        row = TEST_ROW.fullmatch(line)
        if running:
            if current is not None:
                raise ValueError('partial libtest target: missing summary')
            current = {'running': int(running[1]), 'roster': [], 'ignored_roster': []}
        elif summary:
            if current is None:
                raise ValueError('libtest summary without target start')
            counts = dict(zip(('passed', 'failed', 'ignored', 'measured', 'filtered'),
                              map(int, summary.groups()[1:])))
            if (summary[1] != 'ok' or any(counts[k] for k in ('failed', 'measured', 'filtered'))
                    or (counts['ignored'] and not pending_ignored)):
                raise ValueError(f'nonqualifying libtest counts: {counts}')
            if not counts['passed'] + counts['ignored']:
                raise ValueError('zero-test target requires reviewed applicability')
            if (current['running'] != counts['passed'] + counts['ignored']
                    or len(current['roster']) != counts['passed']
                    or len(current['ignored_roster']) != counts['ignored']):
                raise ValueError('partial libtest roster/count mismatch')
            blocks.append({**current, **counts})
            current = None
        elif row:
            if current is None:
                raise ValueError('test row outside a libtest target')
            if row[1] in current['roster'] + current['ignored_roster']:
                raise ValueError(f'duplicate test name in target: {row[1]}')
            if row[2] == 'ok':
                current['roster'].append(row[1])
            elif pending_ignored and (row[2] == 'ignored' or row[2].startswith('ignored, ')):
                current['ignored_roster'].append(row[1])
            else:
                raise ValueError(f'nonpassing test row: {line}')
    if current is not None or len(blocks) != expected_targets:
        raise ValueError('missing or partial libtest target summaries')
    return blocks


def libtest_evidence(stdout, expected_targets):
    """Only nonempty, fully executed rosters qualify; ignored/filtered refuse."""
    return _libtest_blocks(stdout, expected_targets)


def _run_product(argv, root):
    result = subprocess.run(argv, cwd=root, text=True, capture_output=True)
    # Preserve raw evidence before checking exit or the complete named roster.
    sys.stdout.write(result.stdout)
    sys.stdout.flush()
    sys.stderr.write(result.stderr)
    sys.stderr.flush()
    return result


def listed_unit_tests(stdout):
    """Require one complete, nonempty libtest discovery roster."""
    names = []
    summary = None
    for line in stdout.splitlines():
        if not line:
            continue
        row = LIST_ROW.fullmatch(line)
        totals = LIST_SUMMARY.fullmatch(line)
        if row and summary is None:
            name = row[1]
            if name in names:
                raise ValueError(f'duplicate listed unit test: {name}')
            names.append(name)
        elif totals and summary is None:
            summary = (int(totals[1]), int(totals[2]))
        else:
            raise ValueError('unrecognized or incomplete libtest discovery output')
    if summary is None or summary[1] or summary[0] != len(names):
        raise ValueError('partial or non-unit libtest discovery roster')
    if not names:
        raise ValueError('zero-test target requires reviewed applicability')
    return set(names)


def _list_unit_tests(root, package, profile, target, release=False):
    argv = command(package, profile, 'unit', target)
    if release:
        argv.insert(2, '--release')
    argv += ['--', '--list', '--format', 'pretty', '--color', 'never']
    print(json.dumps({'command': argv, 'phase': 'release-list' if release else 'debug-list'}), flush=True)
    result = _run_product(argv, root)
    if result.returncode:
        raise ValueError(f'unit test discovery failed with exit {result.returncode}')
    return listed_unit_tests(result.stdout)


def execute(root, package, profile, kind, provenance):
    active = [target for target, enabled in target_coverage(package, profile) if enabled]
    if not active:
        raise ValueError('zero enabled unit targets requires reviewed applicability')
    targets = active if kind == 'unit' else [None]
    errors = []
    for target in targets:
        argv = command(package, profile, kind, target)
        if kind == 'unit':
            argv += ['--', '--format', 'pretty', '--color', 'never']
        record = {'command': argv, 'package': package['name'], 'target': target,
                  'profile': profile, 'provenance': provenance, 'qualification': 'not evaluated'}
        if kind == 'unit':
            try:
                before = _list_unit_tests(root, package, profile, target)
            except ValueError as error:
                errors.append(f'{target}: {error}')
                record['evidence_error'] = str(error)
                print(json.dumps(record), flush=True)
                continue
        print(json.dumps(record), flush=True)
        result = _run_product(argv, root)
        if result.returncode:
            errors.append(f'{target}: {kind} command failed with exit {result.returncode}')
        elif kind == 'unit':
            try:
                debug = _libtest_blocks(result.stdout, 1, pending_ignored=True)
                debug_names = {name for block in debug for name in block['roster'] + block['ignored_roster']}
                listed = _list_unit_tests(root, package, profile, target)
                if before != listed or listed != debug_names:
                    raise ValueError('debug execution differs from discovered unit roster')
                record['discovered_unit_roster'] = sorted(listed)
                ignored = {name for block in debug for name in block['ignored_roster']}
                if ignored:
                    # Measurement units explicitly require release. Retain the
                    # ordinary debug run, then execute the full release target,
                    # including every ignored test. Neither phase can fail.
                    record['debug_unit_evidence'] = debug
                    release_argv = command(package, profile, 'unit', target)
                    release_argv.insert(2, '--release')
                    release_argv += ['--', '--format', 'pretty', '--color', 'never', '--include-ignored']
                    release_before = _list_unit_tests(root, package, profile, target, release=True)
                    print(json.dumps({**record, 'command': release_argv, 'phase': 'release-all-including-ignored'}), flush=True)
                    release = _run_product(release_argv, root)
                    if release.returncode:
                        raise ValueError(f'release unit command failed with exit {release.returncode}')
                    evidence = libtest_evidence(release.stdout, 1)
                    executed = {name for block in evidence for name in block['roster']}
                    if not ignored <= executed:
                        raise ValueError('ignored unit names missing from complete release execution')
                    release_listed = _list_unit_tests(root, package, profile, target, release=True)
                    if release_before != release_listed or not debug_names <= release_listed or release_listed != executed:
                        raise ValueError('release execution differs from discovered unit roster')
                    record['release_command'] = release_argv
                    record['resolved_ignored_roster'] = sorted(ignored)
                    record['release_discovered_unit_roster'] = sorted(release_listed)
                    record['unit_evidence'] = evidence
                else:
                    record['unit_evidence'] = libtest_evidence(result.stdout, 1)
            except ValueError as error:
                errors.append(f'{target}: {error}')
                record['evidence_error'] = str(error)
        print(json.dumps(record), flush=True)
    if errors:
        raise ValueError('\n'.join(errors))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['matrix', 'inventory', 'run'])
    parser.add_argument('--root', type=Path, default=Path.cwd())
    parser.add_argument('--metadata-json', type=Path, help='Offline inspection only; never execution evidence')
    parser.add_argument('--expected-head')
    parser.add_argument('--expected-base')
    parser.add_argument('--baseline-root', type=Path)
    parser.add_argument('--package')
    parser.add_argument('--profile')
    parser.add_argument('--kind', choices=['unit', 'clippy'])
    args = parser.parse_args(argv)
    provided = (args.expected_head, args.expected_base, args.baseline_root)
    if any(provided) and not all(provided):
        parser.error('identity triple must be complete: --expected-head --expected-base --baseline-root')
    if args.action == 'run' and (args.metadata_json is not None or not all(provided)):
        parser.error('run requires fresh metadata and --expected-head --expected-base --baseline-root')
    if not any(provided) and args.metadata_json is None:
        parser.error('inventory/matrix without identities require offline --metadata-json inspection')
    if args.metadata_json is not None and any(provided):
        parser.error('offline metadata inspection cannot provide verified execution identities')
    provenance = ({'inspection_only': True} if not any(provided) else
                  verify_identity(args.root, *provided))
    packages = load(args.root, args.metadata_json)
    comparison = None
    if all(provided):
        comparison = compare_inventory(load(args.baseline_root), packages)
        # Metadata generation must not silently change the evidence inputs.
        provenance = verify_identity(args.root, *provided)
    if args.action == 'inventory':
        print(json.dumps({'packages': packages, 'provenance': provenance,
                          'baseline_comparison': comparison, 'execution_selection': 'complete workspace',
                          'qualification': 'not evaluated'}, sort_keys=True, indent=2))
    elif args.action == 'matrix':
        # stdout stays one matrix JSON value for GITHUB_OUTPUT; provenance is stderr.
        print(json.dumps({'provenance': provenance, 'baseline_comparison': comparison}), file=sys.stderr)
        print(json.dumps(matrix(packages), separators=(',', ':')))
    else:
        package = next((p for p in packages if p['name'] == args.package), None)
        if package is None or args.kind is None:
            parser.error('run requires a workspace --package, --profile and --kind')
        # Recheck identity immediately before launching product commands.
        provenance = verify_identity(args.root, *provided)
        try:
            execute(args.root, package, args.profile, args.kind, provenance)
        finally:
            # Check after both a passing and a failed product command. The
            # commit IDs alone do not describe an edited working tree.
            verify_identity(args.root, *provided)


if __name__ == '__main__':
    main()
