"""Pure baseline, identity and libtest evidence fixtures; never invoke Cargo."""
import contextlib
import copy
import importlib.util
import io
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('evidence_plan', Path(__file__).parents[1] / 'plan.py')
plan = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(plan)

HEAD = '1' * 40
BASE = '2' * 40


def package():
    return {'name': 'leaf', 'directory': 'crates/leaf', 'features': {'cli': []},
            'dependencies': [], 'targets': [
                {'name': 'leaf', 'kind': ['lib'], 'required-features': [], 'harness': True, 'test': True},
                {'name': 'tool', 'kind': ['bin'], 'required-features': ['cli'], 'harness': True, 'test': True}]}


def output(passed=2, failed=0, ignored=0, measured=0, filtered=0, roster=None):
    names = roster if roster is not None else ['alpha', 'beta'][:passed]
    rows = ''.join(f'test {name} ... ok\n' for name in names)
    return (f'\nrunning {passed + failed + ignored + measured} tests\n' + rows +
            f'\ntest result: ok. {passed} passed; {failed} failed; {ignored} ignored; '
            f'{measured} measured; {filtered} filtered out; finished in 0.00s\n')


class IdentityTests(unittest.TestCase):
    def setUp(self):
        discovery=patch.object(plan, '_list_unit_tests', return_value={'alpha', 'beta'})
        discovery.start()
        self.addCleanup(discovery.stop)

    @patch.object(plan.subprocess, 'check_output')
    def test_exact_head_and_base_are_read_using_argv(self, command):
        command.side_effect = [HEAD + '\n', BASE + '\n']
        identities = plan.verify_identity('/head', HEAD, BASE, '/base')
        self.assertEqual(HEAD, identities['actual_head'])
        self.assertEqual(BASE, identities['actual_base'])
        self.assertEqual(['git', '-C', '/head', 'rev-parse', '--verify', 'HEAD'], command.call_args_list[0].args[0])
        self.assertNotIn('shell', command.call_args_list[0].kwargs)

    @patch.object(plan.subprocess, 'check_output')
    def test_stale_head_refuses(self, command):
        command.side_effect = ['3' * 40, BASE]
        with self.assertRaisesRegex(ValueError, 'stale head'):
            plan.verify_identity('/head', HEAD, BASE, '/base')

    @patch.object(plan.subprocess, 'check_output')
    def test_wrong_base_refuses(self, command):
        command.side_effect = [HEAD, '3' * 40]
        with self.assertRaisesRegex(ValueError, 'wrong baseline'):
            plan.verify_identity('/head', HEAD, BASE, '/base')

    @patch.object(plan.subprocess, 'run')
    def test_execution_without_complete_identity_is_rejected_before_subprocess(self, run):
        with contextlib.redirect_stderr(io.StringIO()):
            for argv in (['run'], ['run', '--expected-head', HEAD], ['inventory'],
                         ['matrix', '--expected-head', HEAD, '--metadata-json', 'fixture.json']):
                with self.subTest(argv=argv), self.assertRaises(SystemExit):
                    plan.main(argv)
        run.assert_not_called()

    @patch.object(plan.subprocess, 'run')
    @patch.object(plan, 'load')
    @patch.object(plan.subprocess, 'check_output')
    def test_cli_rechecks_identity_and_uses_fresh_baseline_before_execution(self, git, load, run):
        git.side_effect = [HEAD, BASE, HEAD, BASE]
        load.side_effect = [[package()], [package()]]
        run.return_value = subprocess.CompletedProcess([], 0, stdout=output(), stderr='')
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            plan.main(['run', '--root', '/head', '--expected-head', HEAD, '--expected-base', BASE,
                       '--baseline-root', '/base', '--package', 'leaf', '--profile', 'default', '--kind', 'unit'])
        self.assertEqual(4, git.call_count)
        self.assertEqual(Path('/base'), load.call_args_list[1].args[0])
        self.assertEqual(1, run.call_count)

    @patch.object(plan.subprocess, 'run')
    @patch.object(plan, 'load')
    @patch.object(plan.subprocess, 'check_output')
    def test_cli_baseline_shrink_blocks_before_product_execution(self, git, load, run):
        git.side_effect = [HEAD, BASE]
        load.side_effect = [[], [package()]]
        with self.assertRaisesRegex(ValueError, 'package removal'):
            plan.main(['run', '--root', '/head', '--expected-head', HEAD, '--expected-base', BASE,
                       '--baseline-root', '/base', '--package', 'leaf', '--profile', 'default', '--kind', 'unit'])
        run.assert_not_called()

    @patch.object(plan.subprocess, 'check_output')
    def test_abbreviated_identity_refuses_before_git(self, command):
        with self.assertRaisesRegex(ValueError, '40-hex'):
            plan.verify_identity('/head', '123abcd', BASE, '/base')
        command.assert_not_called()


class BaselineTests(unittest.TestCase):
    def setUp(self):
        self.base = [package()]
        self.head = copy.deepcopy(self.base)

    def test_unchanged_inventory_is_not_a_qualification_verdict(self):
        report = plan.compare_inventory(self.base, self.head)
        self.assertEqual('not evaluated', report['qualification'])
        self.assertEqual([], report['changed_packages'])

    def test_baseline_package_shrink_refuses(self):
        with self.assertRaisesRegex(ValueError, 'package removal'):
            plan.compare_inventory(self.base, [])

    def test_baseline_feature_shrink_refuses(self):
        self.head[0]['features'] = {}
        with self.assertRaisesRegex(ValueError, 'feature removal'):
            plan.compare_inventory(self.base, self.head)

    def test_baseline_target_shrink_refuses(self):
        self.head[0]['targets'].pop()
        with self.assertRaisesRegex(ValueError, 'target removal'):
            plan.compare_inventory(self.base, self.head)

    def test_formerly_enabled_harness_cannot_be_disabled(self):
        self.head[0]['targets'][0]['harness'] = False
        with self.assertRaisesRegex(ValueError, 'harness disabled'):
            plan.compare_inventory(self.base, self.head)

    def test_formerly_enabled_profile_target_cannot_be_gated_away(self):
        self.head[0]['targets'][0]['required-features'] = ['cli']
        with self.assertRaisesRegex(ValueError, 'formerly enabled unit target lost'):
            plan.compare_inventory(self.base, self.head)

    def test_dependency_change_is_reported_and_keeps_full_selection(self):
        self.head[0]['dependencies'] = ['added-dependency']
        report = plan.compare_inventory(self.base, self.head)
        self.assertEqual('complete workspace', report['execution_selection'])
        self.assertEqual(['added-dependency'], report['changed_packages'][0]['after']['dependencies'])

    def test_additions_keep_complete_inventory(self):
        added = package()
        added['name'] = 'new'
        self.head.append(added)
        self.head[0]['features']['new-feature'] = []
        report = plan.compare_inventory(self.base, self.head)
        self.assertEqual(['new'], report['added_packages'])
        self.assertEqual(1, len(report['changed_packages']))


class LibtestTests(unittest.TestCase):
    def setUp(self):
        discovery=patch.object(plan, '_list_unit_tests', return_value={'alpha', 'beta'})
        discovery.start()
        self.addCleanup(discovery.stop)

    def test_nonzero_complete_roster_and_counts_are_retained(self):
        records = plan.libtest_evidence(output(), 1)
        self.assertEqual(['alpha', 'beta'], records[0]['roster'])
        self.assertEqual(2, records[0]['passed'])

    def test_zero_test_target_requires_reviewed_applicability(self):
        with self.assertRaisesRegex(ValueError, 'zero-test target requires reviewed applicability'):
            plan.libtest_evidence(output(passed=0), 1)

    def test_missing_summary_refuses(self):
        with self.assertRaisesRegex(ValueError, 'missing or partial'):
            plan.libtest_evidence('running 2 tests\ntest alpha ... ok\n', 1)

    def test_partial_target_count_refuses(self):
        with self.assertRaisesRegex(ValueError, 'missing or partial'):
            plan.libtest_evidence(output(), 2)

    def test_partial_roster_refuses(self):
        with self.assertRaisesRegex(ValueError, 'roster/count mismatch'):
            plan.libtest_evidence(output(roster=['alpha']), 1)

    def test_duplicate_names_refuse(self):
        with self.assertRaisesRegex(ValueError, 'duplicate test name'):
            plan.libtest_evidence(output(roster=['alpha', 'alpha']), 1)

    def test_filtered_ignored_measured_and_failed_counts_refuse(self):
        for kind in ('filtered', 'ignored', 'measured', 'failed'):
            with self.subTest(kind=kind), self.assertRaisesRegex(ValueError, 'nonqualifying libtest counts'):
                plan.libtest_evidence(output(**{kind: 1}), 1)

    def test_empty_missing_or_unstarted_summary_refuses(self):
        for text in ('', 'cargo exited successfully', output().split('\n')[-2]):
            with self.subTest(text=text), self.assertRaises(ValueError):
                plan.libtest_evidence(text, 1)

    @patch.object(plan.subprocess, 'run')
    def test_execution_binds_each_active_target_to_its_own_roster(self, run):
        run.return_value = subprocess.CompletedProcess([], 0, stdout=output(), stderr='complete cargo stderr\n')
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            plan.execute('/head', package(), 'all-features', 'unit', {'actual_head': HEAD, 'actual_base': BASE})
        self.assertEqual(2, run.call_count)
        self.assertIn('--lib', run.call_args_list[0].args[0])
        self.assertNotIn('--include-ignored', run.call_args_list[0].args[0])
        self.assertNotIn('--bin', run.call_args_list[0].args[0])
        self.assertIn('--bin', run.call_args_list[1].args[0])
        self.assertNotIn('--lib', run.call_args_list[1].args[0])
        self.assertTrue(run.call_args_list[0].kwargs['capture_output'])
        self.assertEqual('complete cargo stderr\n' * 2, stderr.getvalue())
        self.assertIn('unit_evidence', stdout.getvalue())
        self.assertIn(HEAD, stdout.getvalue())

    @patch.object(plan.subprocess, 'run')
    def test_failed_process_preserves_full_stdout_and_stderr(self, run):
        run.return_value = subprocess.CompletedProcess([], 101, stdout='all stdout\n', stderr='all stderr\n')
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            with self.assertRaisesRegex(ValueError, 'exit 101'):
                plan.execute('/head', package(), 'default', 'unit', {'actual_head': HEAD})
        self.assertIn('all stdout\n', stdout.getvalue())
        self.assertEqual('all stderr\n', stderr.getvalue())

    @patch.object(plan.subprocess, 'run')
    def test_zero_library_still_runs_binary_but_remains_a_failure(self, run):
        run.side_effect = [subprocess.CompletedProcess([], 0, stdout=output(passed=0), stderr=''),
                           subprocess.CompletedProcess([], 0, stdout=output(), stderr='')]
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout), self.assertRaisesRegex(ValueError, 'zero-test'):
            plan.execute('/head', package(), 'all-features', 'unit', {'actual_head': HEAD})
        self.assertEqual(2, run.call_count)
        self.assertIn('unit_evidence', stdout.getvalue())
        self.assertIn('evidence_error', stdout.getvalue())

    @patch.object(plan.subprocess, 'run')
    def test_failed_library_does_not_hide_remaining_binary_results(self, run):
        run.side_effect = [subprocess.CompletedProcess([], 101, stdout='library failure\n', stderr='reason\n'),
                           subprocess.CompletedProcess([], 0, stdout=output(), stderr='')]
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaisesRegex(ValueError, 'exit 101'):
                plan.execute('/head', package(), 'all-features', 'unit', {'actual_head': HEAD})
        self.assertEqual(2, run.call_count)


    @patch.object(plan.subprocess, 'run')
    def test_ignored_units_require_complete_release_execution(self, run):
        pending = output(passed=1, ignored=1, roster=['alpha']).replace('test alpha ... ok\n', 'test alpha ... ok\ntest beta ... ignored, release measurement\n')
        run.side_effect = [subprocess.CompletedProcess([], 0, pending, ''), subprocess.CompletedProcess([], 0, output(), '')]
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            plan.execute('/head', package(), 'default', 'unit', {'actual_head': HEAD})
        self.assertEqual(2, run.call_count)
        self.assertIn('--release', run.call_args_list[1].args[0])
        self.assertIn('--include-ignored', run.call_args_list[1].args[0])
        self.assertNotIn('--ignored', run.call_args_list[1].args[0])
        self.assertIn('resolved_ignored_roster', stdout.getvalue())

    @patch.object(plan.subprocess, 'run')
    def test_release_cannot_omit_the_ignored_test_name(self, run):
        pending = output(passed=1, ignored=1, roster=['alpha']).replace('test alpha ... ok\n', 'test alpha ... ok\ntest beta ... ignored\n')
        run.side_effect = [subprocess.CompletedProcess([], 0, pending, ''), subprocess.CompletedProcess([], 0, output(roster=['alpha', 'different']), '')]
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(ValueError, 'ignored unit names missing'):
            plan.execute('/head', package(), 'default', 'unit', {})

    @patch.object(plan.subprocess, 'run')
    def test_release_failure_cannot_resolve_ignored_units(self, run):
        pending = output(passed=1, ignored=1, roster=['alpha']).replace('test alpha ... ok\n', 'test alpha ... ok\ntest beta ... ignored\n')
        run.side_effect = [subprocess.CompletedProcess([], 0, pending, ''), subprocess.CompletedProcess([], 101, 'release failure', '')]
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(ValueError, 'release unit command failed'):
            plan.execute('/head', package(), 'default', 'unit', {})

    @patch.object(plan.subprocess, 'run')
    def test_release_filtered_results_still_refuse(self, run):
        pending = output(passed=1, ignored=1, roster=['alpha']).replace('test alpha ... ok\n', 'test alpha ... ok\ntest beta ... ignored\n')
        run.side_effect = [subprocess.CompletedProcess([], 0, pending, ''), subprocess.CompletedProcess([], 0, output(filtered=1), '')]
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(ValueError, 'nonqualifying libtest counts'):
            plan.execute('/head', package(), 'default', 'unit', {})

    def test_strict_evidence_still_refuses_complete_but_ignored_roster(self):
        pending = output(passed=1, ignored=1, roster=['alpha']).replace('test alpha ... ok\n', 'test alpha ... ok\ntest beta ... ignored\n')
        with self.assertRaisesRegex(ValueError, 'nonpassing test row'):
            plan.libtest_evidence(pending, 1)

    @patch.object(plan.subprocess, 'run')
    def test_incomplete_ignored_roster_cannot_trigger_a_release_pass(self, run):
        run.return_value = subprocess.CompletedProcess([], 0, output(passed=1, ignored=1, roster=['alpha']), '')
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(ValueError, 'roster/count mismatch'):
            plan.execute('/head', package(), 'default', 'unit', {})
        self.assertEqual(1, run.call_count)


class RosterDiscoveryTests(unittest.TestCase):
    def test_complete_nonzero_discovery_roster(self):
        self.assertEqual({'alpha','beta'},plan.listed_unit_tests('alpha: test\nbeta: test\n\n2 tests, 0 benchmarks\n'))

    def test_partial_duplicate_zero_and_benchmark_discovery_refuse(self):
        for raw in ('', '0 tests, 0 benchmarks\n', 'alpha: test\n2 tests, 0 benchmarks\n',
                    'alpha: test\nalpha: test\n2 tests, 0 benchmarks\n',
                    'alpha: test\n1 test, 1 benchmark\n',
                    'alpha: test\n1 test, 0 benchmarks\nunexpected output\n'):
            with self.subTest(raw=raw),self.assertRaises(ValueError):
                plan.listed_unit_tests(raw)

    @patch.object(plan.subprocess, 'run')
    def test_debug_execution_must_match_discovery_names(self, run):
        listed='alpha: test\ndifferent: test\n\n2 tests, 0 benchmarks\n'
        run.side_effect=[subprocess.CompletedProcess([],0,listed,''),subprocess.CompletedProcess([],0,output(),''),
                         subprocess.CompletedProcess([],0,listed,'')]
        with contextlib.redirect_stdout(io.StringIO()),self.assertRaisesRegex(ValueError,'debug execution differs'):
            plan.execute('/head',package(),'default','unit',{})
        self.assertIn('--list',run.call_args_list[0].args[0])
        self.assertNotIn('--release',run.call_args_list[0].args[0])

    @patch.object(plan.subprocess, 'run')
    def test_release_must_discover_and_execute_all_debug_names(self, run):
        pending=output(passed=1,ignored=1,roster=['alpha']).replace('test alpha ... ok\n','test alpha ... ok\ntest beta ... ignored\n')
        listed='alpha: test\nbeta: test\n\n2 tests, 0 benchmarks\n'
        run.side_effect=[subprocess.CompletedProcess([],0,listed,''),subprocess.CompletedProcess([],0,pending,''),
                         subprocess.CompletedProcess([],0,listed,''),subprocess.CompletedProcess([],0,listed,''),
                         subprocess.CompletedProcess([],0,output(),''),
                         subprocess.CompletedProcess([],0,'beta: test\ndifferent: test\n\n2 tests, 0 benchmarks\n','')]
        with contextlib.redirect_stdout(io.StringIO()),self.assertRaisesRegex(ValueError,'release execution differs'):
            plan.execute('/head',package(),'default','unit',{})
        self.assertIn('--list',run.call_args_list[3].args[0])
        self.assertIn('--release',run.call_args_list[3].args[0])

    @patch.object(plan.subprocess, 'run')
    def test_failed_discovery_is_not_execution_evidence(self, run):
        run.side_effect=[subprocess.CompletedProcess([],101,'','failed list')]
        with contextlib.redirect_stdout(io.StringIO()),contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaisesRegex(ValueError,'discovery failed with exit 101'):
                plan.execute('/head',package(),'default','unit',{})

    @patch.object(plan.subprocess, 'run')
    def test_changing_discovery_roster_after_execution_refuses(self, run):
        listed='alpha: test\nbeta: test\n\n2 tests, 0 benchmarks\n'
        changed='alpha: test\ndifferent: test\n\n2 tests, 0 benchmarks\n'
        run.side_effect=[subprocess.CompletedProcess([],0,listed,''),subprocess.CompletedProcess([],0,output(),''),
                         subprocess.CompletedProcess([],0,changed,'')]
        with contextlib.redirect_stdout(io.StringIO()),self.assertRaisesRegex(ValueError,'debug execution differs'):
            plan.execute('/head',package(),'default','unit',{})


if __name__ == '__main__':
    unittest.main()
