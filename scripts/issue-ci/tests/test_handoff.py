"""Affected controller/policy regression tests; no network or product tests."""
import hashlib
import importlib.util
import json
import re
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]

def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

ci = load("handoff", ROOT / "scripts/issue-ci/handoff.py")
guard = load("guard", ROOT / "scripts/issue-ci/check-policy.py")
CONFIG = json.loads((ROOT / ".github/issue-ci-required-gates.json").read_text())
GATES = CONFIG["workflows"]
SHA = "a" * 40

def evidence():
    return {w["path"]: (
        dict(id=i, run_number=i, run_attempt=1, status="completed", conclusion="success", html_url=f"https://github.com/o/r/actions/runs/{i}"),
        [dict(name=n, status="completed", conclusion="success", html_url=f"https://github.com/o/r/actions/runs/{i}/job/{j}") for j, n in enumerate(w["jobs"])])
        for i, w in enumerate(GATES, 1)}

class PolicyTests(unittest.TestCase):
    def test_pinned_controller(self):
        self.assertEqual(hashlib.sha256((ROOT / "scripts/issue-ci/handoff.py").read_bytes()).hexdigest(), "1abb5570032eaf294a50c5fa0bfb9ae7edd1db7e3f0a98a1f56601720916caf1")
        self.assertEqual(CONFIG["aw_revision"], "84e9b72b4a63cc30b47449a2e321981525fa9c34")

    def test_rollout_blocked_even_if_existing_gates_green(self):
        self.assertEqual(ci.evaluate(GATES, evidence())[0], "done")
        with self.assertRaises(RuntimeError):
            guard.check(ROOT / ".github/issue-ci-required-gates.json")

    def test_guard_refuses_absent_malformed_and_nonempty_blockers(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "policy.json"
            for blockers in [None, "", ["e2e decision pending"]]:
                path.write_text(json.dumps(dict(workflows=GATES, qualification_blockers=blockers)))
                with self.subTest(blockers=blockers), self.assertRaises(RuntimeError): guard.check(path)
            path.write_text(json.dumps(dict(workflows=GATES, qualification_blockers=[])))
            guard.check(path)
            path.write_text(json.dumps(dict(workflows=[], qualification_blockers=[])))
            with self.assertRaises(RuntimeError): guard.check(path)

    def test_exact_live_gate_contract(self):
        expected = [{'path': '.github/workflows/keep-ci.yml', 'jobs': ['fmt, clippy, tests, release contract']}]
        self.assertEqual(GATES, expected)
        source = (ROOT / expected[0]["path"]).read_text()
        self.assertEqual(re.findall(r"^    name: (.+)$", source, re.M), expected[0]["jobs"])
        workflow = (ROOT / ".github/workflows/issue-ci.yml").read_text()
        self.assertIn('workflows: [\"keep-ci\"]', workflow)
        self.assertLess(workflow.index("check-policy.py"), workflow.index("handoff.py"))
        self.assertIn("ref: ${{ github.event.repository.default_branch }}", workflow)
        self.assertIn("persist-credentials: false", workflow)
        self.assertNotIn("pull_request_target", workflow)
        self.assertNotIn("download-artifact", workflow)

    def test_missing_workflow_or_job_never_passes(self):
        self.assertEqual(ci.evaluate(GATES, {})[0], "pending")
        for index in range(len(GATES[0]["jobs"])):
            data = evidence()
            data[GATES[0]["path"]][1].pop(index)
            self.assertEqual(ci.evaluate(GATES, data)[0], "pending")

    def test_skipped_cancelled_neutral_and_pending_never_pass(self):
        for conclusion in ["skipped", "cancelled", "neutral", None]:
            data = evidence()
            data[GATES[0]["path"]][1][0]["conclusion"] = conclusion
            self.assertEqual(ci.evaluate(GATES, data)[0], "pending")
        data = evidence()
        data[GATES[0]["path"]][0]["status"] = "in_progress"
        self.assertEqual(ci.evaluate(GATES, data)[0], "pending")

    def test_failure_returns_todo_and_exact_link(self):
        data = evidence()
        job = data[GATES[0]["path"]][1][0]
        job["conclusion"] = "failure"
        state, reasons, _ = ci.evaluate(GATES, data)
        self.assertEqual(state, "todo")
        self.assertIn(job["html_url"], reasons[0])

    def test_stale_foreign_runs_and_old_attempt_cannot_fill_gaps(self):
        run = dict(id=3, run_number=2, run_attempt=2, head_sha=SHA, event="pull_request", status="completed", conclusion="success", html_url="https://github.com/o/r/actions/runs/3", head_repository={"full_name":"o/r"}, path=GATES[0]["path"])
        stale = dict(run, id=100, head_sha="b" * 40)
        foreign = dict(run, id=101, head_repository={"full_name":"fork/r"})
        old = dict(run, id=2, run_attempt=1)
        with patch.object(ci, "pages", side_effect=[[old, run, stale, foreign], []]) as pages:
            result = ci.snapshot("o/r", SHA, {"workflows": [GATES[0]]})
        self.assertEqual(result[GATES[0]["path"]][0]["id"], 3)
        self.assertIn("/runs/3/attempts/2/jobs", pages.call_args_list[1].args[0])
        self.assertEqual(ci.evaluate(GATES, result)[0], "pending")

if __name__ == "__main__": unittest.main()
