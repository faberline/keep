#!/usr/bin/env python3
"""Validate the trusted failure-only rollout without blocking real failures."""
import importlib.util
from pathlib import Path
import sys


def load_controller():
    path = Path(__file__).with_name('handoff.py')
    spec = importlib.util.spec_from_file_location('issue_ci_handoff', path)
    controller = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(controller)
    return controller


def check(path):
    controller = load_controller()
    if not all(callable(getattr(controller, name, None)) for name in ('load_config', 'evaluate_policy')):
        raise RuntimeError('failure-only rollout requires the reviewed policy-aware controller')
    policy = controller.load_config(path)
    if policy.get('lifecycle_mode') != 'failure-only':
        raise RuntimeError('this rollout requires explicit lifecycle_mode failure-only')
    if not isinstance(policy.get('qualification_blockers'), list) or not policy['qualification_blockers']:
        raise RuntimeError('this rollout requires an explicit nonempty qualification_blockers list')
    return policy


if __name__ == '__main__':
    check(sys.argv[1])
