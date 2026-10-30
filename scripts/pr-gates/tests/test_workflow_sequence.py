"""Protect the PR unit-before-integration dependency and main push path."""
from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[3]


def job(source, name):
    match = re.search(rf'^  {re.escape(name)}:\n(.*?)(?=^  [a-z][a-z-]*:\n|\Z)',
                      source, re.MULTILINE | re.DOTALL)
    if not match:
        raise AssertionError(f'missing workflow job: {name}')
    return match.group(1)


class WorkflowSequence(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.unit = (ROOT / '.github/workflows/pr-unit-static.yml').read_text()
        cls.ci = (ROOT / '.github/workflows/keep-ci.yml').read_text()

    def test_separate_unit_and_ci_runs_cannot_cancel_each_other(self):
        triggers = self.unit.split('\npermissions:', 1)[0]
        self.assertRegex(triggers, r'(?m)^  pull_request:$')
        self.assertRegex(triggers, r'(?m)^  workflow_call:$')
        self.assertIn('keep-pr-unit-static-${{ github.workflow }}-${{ github.ref }}', self.unit)
        prerequisite = job(self.ci, 'unit-static')
        self.assertIn("if: github.event_name == 'pull_request'", prerequisite)
        self.assertIn('uses: ./.github/workflows/pr-unit-static.yml', prerequisite)

    def test_pr_tests_and_release_contract_wait_for_unit_success(self):
        gates = job(self.ci, 'gates')
        self.assertIn('needs: unit-static', gates)
        self.assertIn("(github.event_name != 'pull_request' || needs.unit-static.result == 'success')", gates)
        self.assertIn('!cancelled()', gates)
        for command in ('cargo fmt --all -- --check',
                        'cargo clippy --locked --workspace --all-targets --all-features -- -D warnings',
                        'cargo test --locked --workspace',
                        'scripts/release/verify-release-contract.py --app keep'):
            self.assertIn(command, gates)

    def test_main_push_keeps_existing_gate(self):
        triggers = self.ci.split('\npermissions:', 1)[0]
        self.assertRegex(triggers, r'(?m)^  push:$')
        self.assertRegex(triggers, r'(?m)^    branches: \[main\]$')
        self.assertIn("github.event_name != 'pull_request'", job(self.ci, 'gates'))


if __name__ == '__main__':
    unittest.main()
