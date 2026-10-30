"""Pure fixtures; no Cargo/product/test/network subprocesses."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('plan', Path(__file__).parents[1] / 'plan.py')
plan = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(plan)


def package(name='leaf', dependencies=(), features=None, targets=None):
    return {'name': name, 'directory': 'crates/' + name,
            'features': features or {}, 'dependencies': list(dependencies),
            'targets': targets or [{'name': name, 'kind': ['lib'], 'required-features': []}]}


class MatrixTests(unittest.TestCase):
    def test_default_rows_come_first_without_losing_feature_rows(self):
        rows = plan.matrix([package('a', features={'default': ['x'], 'x': []}), package('b')])['include']
        self.assertEqual([{'package': 'a', 'profile': 'default'}, {'package': 'b', 'profile': 'default'}], rows[:2])
        self.assertEqual({('a', 'default'), ('a', 'no-default'), ('a', 'feature:x'), ('a', 'all-features'), ('b', 'default')},
                         {(r['package'], r['profile']) for r in rows})
        self.assertEqual(5, len(rows))

    def test_matrix_limit_refuses_instead_of_dropping_packages(self):
        with self.assertRaisesRegex(ValueError, 'without dropping rows'):
            plan.matrix([package(str(n)) for n in range(257)])

    def test_featureless_package_has_real_default_lib_command(self):
        p = package()
        self.assertEqual(['default'], plan.profiles(p))
        self.assertEqual(['cargo', 'test', '--locked', '-p', 'leaf', '--lib'], plan.command(p, 'default', 'unit'))

    def test_named_features_run_without_default_unification(self):
        p = package(features={'default': ['full'], 'full': ['render-only'], 'render-only': []})
        self.assertEqual(['default', 'no-default', 'feature:full', 'feature:render-only', 'all-features'], plan.profiles(p))
        self.assertEqual(['--no-default-features', '--features', 'render-only'], plan.command(p, 'feature:render-only', 'unit')[-3:])
        self.assertEqual({'full', 'render-only', 'default'}, plan.enabled_features(p, 'default'))
        self.assertEqual(set(), plan.enabled_features(p, 'no-default'))

    def test_feature_required_binary_is_in_all_features_command(self):
        p = package(features={'cli': []}, targets=[{'name': 'leaf', 'kind': ['lib'], 'required-features': []},
                    {'name': 'tool', 'kind': ['bin'], 'required-features': ['cli']}])
        self.assertNotIn('--bin', plan.command(p, 'default', 'unit'))
        self.assertEqual(['--lib', '--bin', 'tool', '--all-features'], plan.command(p, 'all-features', 'unit')[5:])
        self.assertEqual([True, False], [active for _, active in plan.target_coverage(p, 'default')])
        self.assertEqual([True, True], [active for _, active in plan.target_coverage(p, 'all-features')])

    def test_optional_dependency_feature_selects_its_required_binary(self):
        p=package(features={'alias': [], 'direct': ['alias/extra'], 'nested': ['direct'],
                            'weak': ['alias?/extra'], 'internal': ['dep:alias']},
                  targets=[{'name':'leaf','kind':['lib'],'required-features':[]},
                           {'name':'tool','kind':['bin'],'required-features':['alias']}])
        p['optional_dependencies']=['alias']
        for profile in ('feature:direct','feature:nested'):
            self.assertIn('--bin',plan.command(p,profile,'unit'))
        for profile in ('feature:weak','feature:internal','no-default'):
            self.assertNotIn('--bin',plan.command(p,profile,'unit'))
        p['optional_dependencies']=[]
        self.assertNotIn('--bin',plan.command(p,'feature:direct','unit'))

    def test_binary_only_profile_cannot_succeed_with_zero_targets(self):
        p = package(features={'cli': []}, targets=[{'name': 'tool', 'kind': ['bin'], 'required-features': ['cli']}])
        with self.assertRaisesRegex(ValueError, 'zero enabled'):
            plan.command(p, 'default', 'unit')

    def test_unknown_profile_and_command_kind_refuse(self):
        with self.assertRaises(ValueError):
            plan.command(package(), 'mystery', 'unit')
        with self.assertRaises(ValueError):
            plan.command(package(), 'default', 'qualify')

    def test_clippy_covers_all_targets_with_warnings_denied(self):
        argv = plan.command(package(), 'default', 'clippy')
        self.assertIn('--all-targets', argv)
        self.assertEqual(['--', '-D', 'warnings'], argv[-3:])


class SelectionTests(unittest.TestCase):
    def setUp(self):
        self.packages = [package('a'), package('b', ['a']), package('c', ['b']), package('unrelated')]

    def test_reverse_dependencies_are_transitive(self):
        self.assertEqual(['a', 'b', 'c'], plan.affected(self.packages, ['crates/a/src/lib.rs']))

    def test_unknown_docs_root_and_empty_changes_fallback_full(self):
        for paths in (['docs/readme.md'], ['Cargo.toml'], ['crates/deleted/src/lib.rs'], []):
            with self.subTest(paths=paths):
                self.assertEqual(['a', 'b', 'c', 'unrelated'], plan.affected(self.packages, paths))

    def test_crate_path_prefix_does_not_match_neighbor(self):
        self.assertEqual(['a', 'b', 'c', 'unrelated'], plan.affected(self.packages, ['crates/ab/src/lib.rs']))


class InventoryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'Cargo.toml').write_text('[workspace]\nmembers = ["crates/*"]\n')
        (self.root / 'crates/leaf').mkdir(parents=True)
        (self.root / 'crates/leaf/Cargo.toml').write_text('[package]\nname="leaf"\nversion="0.1.0"\n')
        self.metadata = {'workspace_members': ['leaf-id'], 'packages': [{
            'name': 'leaf', 'id': 'leaf-id', 'manifest_path': str(self.root / 'crates/leaf/Cargo.toml'),
            'features': {}, 'targets': [{'name': 'leaf', 'kind': ['lib'], 'test': True}], 'dependencies': []}]}

    def add_package(self, name, directory):
        directory = self.root / directory
        directory.mkdir(parents=True, exist_ok=True)
        manifest = directory / 'Cargo.toml'
        manifest.write_text(f'[package]\nname="{name}"\nversion="0.1.0"\n')
        package = {'name': name, 'id': name+'-id', 'manifest_path': str(manifest),
                   'features': {}, 'targets': [{'name': name, 'kind': ['lib'], 'test': True}],
                   'dependencies': []}
        self.metadata['workspace_members'].append(package['id'])
        self.metadata['packages'].append(package)
        return package

    def test_implicit_transitive_workspace_members_are_included(self):
        auto = self.add_package('auto', 'implicit/auto')
        nested = self.add_package('nested', 'implicit/nested')
        self.metadata['packages'][0]['dependencies'].append(
            {'name':'auto','path':str(self.root/'implicit/auto')})
        auto['dependencies'].append({'name':'nested','path':str(self.root/'implicit/nested')})
        self.assertEqual(['auto','leaf','nested'],[p['name'] for p in plan.inventory(self.root,self.metadata)])
        self.assertEqual('nested',nested['name'])

    def test_missing_reachable_implicit_member_refuses(self):
        self.add_package('auto','implicit/auto')
        self.metadata['packages'][0]['dependencies'].append(
            {'name':'auto','path':str(self.root/'implicit/auto')})
        self.metadata['workspace_members'].remove('auto-id')
        with self.assertRaisesRegex(ValueError,'implicit path member missing'):
            plan.inventory(self.root,self.metadata)

    def test_path_dependency_must_match_target_package_identity(self):
        self.add_package('auto','implicit/auto')
        self.metadata['packages'][0]['dependencies'].append(
            {'name':'different','path':str(self.root/'implicit/auto')})
        with self.assertRaisesRegex(ValueError,'dependency identity mismatch'):
            plan.inventory(self.root,self.metadata)

    def test_unrelated_extra_member_refuses(self):
        self.add_package('unrelated','implicit/unrelated')
        with self.assertRaisesRegex(ValueError,'unexplained workspace member'):
            plan.inventory(self.root,self.metadata)

    def test_excluded_path_dependency_does_not_become_a_member(self):
        excluded=self.root/'implicit/excluded';excluded.mkdir(parents=True)
        (excluded/'Cargo.toml').write_text('[package]\nname="excluded"\nversion="0.1.0"\n')
        (self.root/'Cargo.toml').write_text('[workspace]\nmembers=["crates/*"]\nexclude=["implicit/excluded"]\n')
        self.metadata['packages'][0]['dependencies'].append({'name':'excluded','path':str(excluded)})
        self.assertEqual(['leaf'],[p['name'] for p in plan.inventory(self.root,self.metadata)])

    def test_descendant_of_excluded_directory_is_not_a_member(self):
        excluded=self.root/'implicit/excluded/nested';excluded.mkdir(parents=True)
        (excluded/'Cargo.toml').write_text('[package]\nname="nested"\nversion="0.1.0"\n')
        (self.root/'Cargo.toml').write_text('[workspace]\nmembers=["crates/*"]\nexclude=["implicit/excluded"]\n')
        self.metadata['packages'][0]['dependencies'].append({'name':'nested','path':str(excluded)})
        self.assertEqual(['leaf'],[p['name'] for p in plan.inventory(self.root,self.metadata)])

    def test_explicit_member_takes_precedence_over_excluded_directory(self):
        self.add_package('nested','implicit/excluded/nested')
        (self.root/'Cargo.toml').write_text('[workspace]\nmembers=["crates/*","implicit/excluded/nested"]\n'
                                             'exclude=["implicit/excluded"]\n')
        self.assertEqual(['leaf','nested'],[p['name'] for p in plan.inventory(self.root,self.metadata)])

    def test_excluded_glob_match_is_not_a_member(self):
        excluded=self.root/'implicit/excluded';excluded.mkdir(parents=True)
        (excluded/'Cargo.toml').write_text('[package]\nname="excluded"\nversion="0.1.0"\n')
        (self.root/'Cargo.toml').write_text('[workspace]\nmembers=["crates/*","implicit/*"]\n'
                                             'exclude=["implicit/excluded"]\n')
        self.assertEqual(['leaf'],[p['name'] for p in plan.inventory(self.root,self.metadata)])

    def test_missing_explicit_member_refuses(self):
        (self.root/'Cargo.toml').write_text('[workspace]\nmembers=["crates/*","missing"]\n')
        with self.assertRaisesRegex(ValueError,'declared member missing'):
            plan.inventory(self.root,self.metadata)

    def test_recursive_member_glob_includes_deep_package(self):
        self.add_package('deep','implicit/one/two/unit')
        (self.root/'Cargo.toml').write_text('[workspace]\nmembers=["crates/*","implicit/**/unit"]\n')
        self.assertEqual(['deep','leaf'],[p['name'] for p in plan.inventory(self.root,self.metadata)])

    def test_glob_includes_hidden_workspace_package(self):
        self.add_package('hidden','crates/.hidden')
        self.assertEqual(['hidden','leaf'],[p['name'] for p in plan.inventory(self.root,self.metadata)])

    def test_exclude_glob_is_a_literal_and_does_not_hide_dependency(self):
        self.add_package('skip','implicit/one/two/skip')
        excluded=self.root/'implicit/one/two/skip'
        (self.root/'Cargo.toml').write_text('[workspace]\nmembers=["crates/*"]\nexclude=["implicit/**/skip"]\n')
        self.metadata['packages'][0]['dependencies'].append({'name':'skip','path':str(excluded)})
        self.assertEqual(['leaf','skip'],[p['name'] for p in plan.inventory(self.root,self.metadata)])

    def test_root_package_is_a_workspace_member(self):
        manifest=self.root/'Cargo.toml'
        manifest.write_text('[package]\nname="root"\nversion="0.1.0"\n[workspace]\nmembers=["crates/*"]\n')
        self.metadata['workspace_members'].append('root-id')
        self.metadata['packages'].append({'name':'root','id':'root-id','manifest_path':str(manifest),
            'features':{},'targets':[{'name':'root','kind':['lib'],'test':True}],'dependencies':[]})
        self.assertEqual(['leaf','root'],[p['name'] for p in plan.inventory(self.root,self.metadata)])

    def test_root_package_can_be_the_only_declared_member(self):
        manifest=self.root/'Cargo.toml'
        manifest.write_text('[package]\nname="root"\nversion="0.1.0"\n[workspace]\n')
        self.metadata={'workspace_members':['root-id'],'packages':[{'name':'root','id':'root-id',
            'manifest_path':str(manifest),'features':{},'targets':[{'name':'root','kind':['lib'],'test':True}],
            'dependencies':[]}]}
        self.assertEqual(['root'],[p['name'] for p in plan.inventory(self.root,self.metadata)])

    def test_duplicate_member_identity_refuses(self):
        self.metadata['workspace_members'].append('leaf-id')
        with self.assertRaisesRegex(ValueError,'package identity'):
            plan.inventory(self.root,self.metadata)

    def test_complete_inventory_preserves_target(self):
        self.assertEqual('leaf', plan.inventory(self.root, self.metadata)[0]['targets'][0]['name'])

    def test_partial_inventory_refuses(self):
        self.metadata['packages'] = []
        with self.assertRaisesRegex(ValueError, 'partial workspace inventory'):
            plan.inventory(self.root, self.metadata)

    def test_renamed_dependency_preserves_actual_package_identity(self):
        self.metadata['packages'][0]['dependencies'] = [{'name': 'actual', 'rename': 'local_alias'}]
        self.assertEqual(['actual'], plan.inventory(self.root, self.metadata)[0]['dependencies'])

    def test_optional_dependency_uses_local_alias_for_feature_resolution(self):
        self.metadata['packages'][0]['dependencies']=[{'name':'actual','rename':'local_alias','optional':True}]
        self.assertEqual(['local_alias'],plan.inventory(self.root,self.metadata)[0]['optional_dependencies'])

    def test_missing_package_identity_refuses(self):
        self.metadata['workspace_members'].append('missing-id')
        with self.assertRaisesRegex(ValueError, 'package identity'):
            plan.inventory(self.root, self.metadata)

    def test_unreachable_binary_required_feature_refuses_before_scheduling(self):
        self.metadata['packages'][0]['targets'].append({
            'name': 'unreachable-tool', 'kind': ['bin'], 'test': True,
            'required-features': ['unknown-feature']})
        with self.assertRaisesRegex(ValueError, 'unreachable library/binary'):
            plan.inventory(self.root, self.metadata)

    def test_zero_target_profile_refuses_before_scheduling(self):
        self.metadata['packages'][0]['features'] = {'cli': []}
        self.metadata['packages'][0]['targets'] = [{
            'name': 'tool', 'kind': ['bin'], 'test': True, 'required-features': ['cli']}]
        with self.assertRaisesRegex(ValueError, 'zero enabled unit targets'):
            plan.inventory(self.root, self.metadata)

    def test_manifest_harness_false_refuses_even_when_metadata_says_test_true(self):
        path = self.root / 'crates/leaf/Cargo.toml'
        path.write_text(path.read_text() + '\n[lib]\nharness=false\n')
        with self.assertRaisesRegex(ValueError, 'disabled library/binary unit harness in manifest'):
            plan.inventory(self.root, self.metadata)

    def test_manifest_binary_harness_false_refuses(self):
        path = self.root / 'crates/leaf/Cargo.toml'
        path.write_text(path.read_text() + '\n[[bin]]\nname="tool"\nharness=false\n')
        with self.assertRaisesRegex(ValueError, 'disabled library/binary unit harness in manifest'):
            plan.inventory(self.root, self.metadata)

    def test_disabled_harness_refuses(self):
        self.metadata['packages'][0]['targets'][0]['test'] = False
        with self.assertRaisesRegex(ValueError, 'disabled'):
            plan.inventory(self.root, self.metadata)


if __name__ == '__main__':
    unittest.main()
