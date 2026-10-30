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
        self.assertEqual(hashlib.sha256((ROOT / "scripts/issue-ci/handoff.py").read_bytes()).hexdigest(), "c67533b39d45ad62d407af3748856be396941647c349b63464424fa6aff1c198")
        self.assertEqual(CONFIG["ci_handoff_policy"]["aw_revision"], "67af8d020df3ca2ec4b064289408cfb0c2f8b299")
        self.assertEqual(CONFIG["aw_revision"], "8e2e93e5ba8feb30b6668de39139980ea3bb085c")

    def test_failure_only_policy_never_done_even_if_existing_gates_green(self):
        guard.check(ROOT / ".github/issue-ci-required-gates.json")
        self.assertEqual(CONFIG["lifecycle_mode"], "failure-only")
        self.assertTrue(CONFIG["qualification_blockers"])
        self.assertEqual(ci.evaluate_policy(CONFIG, evidence())[0], "pending")
        self.assertEqual(ci.evaluate_policy(dict(CONFIG, qualification_blockers=[]), evidence())[0], "pending")
        self.assertEqual(ci.evaluate_policy(dict(CONFIG, lifecycle_mode="full"), evidence())[0], "pending")

    def test_guard_refuses_missing_malformed_empty_blockers_and_full_mode(self):
        invalid = [dict(CONFIG, qualification_blockers=value) for value in [None, "", [], [None], [" "]]]
        missing = dict(CONFIG)
        missing.pop("qualification_blockers")
        invalid += [missing, dict(CONFIG, lifecycle_mode="full"), dict(CONFIG, workflows=[])]
        missing = dict(CONFIG)
        missing.pop("lifecycle_mode")
        invalid.append(missing)
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "policy.json"
            for config in invalid:
                path.write_text(json.dumps(config))
                with self.subTest(config=config), self.assertRaises(RuntimeError): guard.check(path)

    def test_guard_refuses_old_controller_without_policy_api(self):
        from types import SimpleNamespace
        with patch.object(guard, "load_controller", return_value=SimpleNamespace(load_config=ci.load_config)):
            with self.assertRaises(RuntimeError): guard.check(ROOT / ".github/issue-ci-required-gates.json")

    def test_exact_live_gate_contract(self):
        expected = [{'path': '.github/workflows/keep-ci.yml', 'jobs': ['fmt, clippy, tests, release contract']}]
        self.assertEqual(GATES, expected)
        source = (ROOT / expected[0]["path"]).read_text()
        self.assertEqual(re.findall(r"^    name: (.+)$", source, re.M), expected[0]["jobs"])
        workflow = (ROOT / ".github/workflows/issue-ci.yml").read_text()
        self.assertIn('workflows: ["keep-ci", "Claude final review"]', workflow)
        self.assertIn('group: issue-ci-handoff', workflow)
        self.assertIn('branches: [main]', workflow)
        self.assertIn('faberline/aw/.github/actions/native-auto-merge@8e2e93e5ba8feb30b6668de39139980ea3bb085c', workflow)
        self.assertNotIn('actions/checkout@', workflow)
        self.assertNotIn('download-artifact', workflow)
        self.assertNotIn('CLAUDE_CODE_OAUTH_TOKEN', workflow)
        self.assertEqual(CONFIG['ci_handoff_policy']['workflows'], GATES)
        self.assertEqual(CONFIG['ci_handoff_policy']['lifecycle_mode'], 'failure-only')
        self.assertEqual(CONFIG['ci_handoff_policy']['qualification_blockers'], CONFIG['qualification_blockers'])

    def test_missing_workflow_or_job_never_passes(self):
        self.assertEqual(ci.evaluate_policy(CONFIG, {})[0], "pending")
        for index in range(len(GATES[0]["jobs"])):
            data = evidence()
            data[GATES[0]["path"]][1].pop(index)
            self.assertEqual(ci.evaluate_policy(CONFIG, data)[0], "pending")

    def test_skipped_cancelled_neutral_and_pending_never_pass(self):
        for conclusion in ["skipped", "cancelled", "neutral", None]:
            data = evidence()
            data[GATES[0]["path"]][1][0]["conclusion"] = conclusion
            self.assertEqual(ci.evaluate_policy(CONFIG, data)[0], "pending")
        data = evidence()
        data[GATES[0]["path"]][0]["status"] = "in_progress"
        self.assertEqual(ci.evaluate_policy(CONFIG, data)[0], "pending")

    def test_failure_returns_todo_and_exact_link(self):
        data = evidence()
        job = data[GATES[0]["path"]][1][0]
        job["conclusion"] = "failure"
        state, reasons, _ = ci.evaluate_policy(CONFIG, data)
        self.assertEqual(state, "todo")
        self.assertIn(job["html_url"], reasons[0])

    def test_privileged_failure_handoff_and_success_no_writes(self):
        from argparse import Namespace
        import contextlib
        import io
        def apply(data, labels):
            issue = dict(state="open", labels=[dict(name=name) for name in labels])
            pr = dict(state="open", body="Closes #7", head=dict(sha=SHA, ref="issue/7", repo=dict(full_name="o/r")))
            comments, writes = [], []
            def api(path, method="GET", data=None):
                if method == "GET": return pr if "/pulls/" in path else issue
                writes.append((path, method, data))
                if method == "POST" and path.endswith("/comments"):
                    comment = dict(id=1, body=data["body"], user=dict(login="github-actions[bot]"))
                    comments.append(comment)
                    return comment
                if method == "PATCH": comments[0]["body"] = data["body"]
                if method == "POST" and path.endswith("/labels"):
                    issue["labels"].extend(dict(name=name) for name in data["labels"])
                if method == "DELETE":
                    issue["labels"] = [label for label in issue["labels"] if label["name"] != path.rsplit("/", 1)[1]]
                return {}
            args = Namespace(repo="o/r", pr=8, head=SHA, apply=True)
            with patch.object(ci, "api", side_effect=api), patch.object(ci, "pages", return_value=comments), patch.object(ci, "snapshot", return_value=data), contextlib.redirect_stdout(io.StringIO()):
                ci.process_head(args, CONFIG, check_event=False)
            return writes, issue, comments
        writes, _, _ = apply(evidence(), {"doing", "priority:p1"})
        self.assertEqual(writes, [])
        failed = evidence()
        failed[GATES[0]["path"]][1][0]["conclusion"] = "failure"
        writes, issue, comments = apply(failed, {"doing", "priority:p1"})
        self.assertEqual({label["name"] for label in issue["labels"]}, {"todo", "priority:p1"})
        self.assertIn(failed[GATES[0]["path"]][1][0]["html_url"], comments[0]["body"])
        writes, _, _ = apply(failed, {"done", "priority:p1"})
        self.assertEqual(writes, [])

    def test_stale_foreign_runs_and_old_attempt_cannot_fill_gaps(self):
        run = dict(id=3, run_number=2, run_attempt=2, head_sha=SHA, event="pull_request", status="completed", conclusion="success", html_url="https://github.com/o/r/actions/runs/3", head_repository={"full_name":"o/r"}, path=GATES[0]["path"])
        stale = dict(run, id=100, head_sha="b" * 40)
        foreign = dict(run, id=101, head_repository={"full_name":"fork/r"})
        old = dict(run, id=2, run_attempt=1)
        with patch.object(ci, "pages", side_effect=[[old, run, stale, foreign], []]) as pages:
            result = ci.snapshot("o/r", SHA, {"workflows": [GATES[0]]})
        self.assertEqual(result[GATES[0]["path"]][0]["id"], 3)
        self.assertIn("/runs/3/attempts/2/jobs", pages.call_args_list[1].args[0])
        self.assertEqual(ci.evaluate_policy(CONFIG, result)[0], "pending")

if __name__ == "__main__": unittest.main()
