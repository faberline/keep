#!/usr/bin/env python3
"""Refuse automatic lifecycle mutations until the reviewed coverage policy is complete."""
import json
from pathlib import Path
import sys


def check(path):
    policy = json.loads(Path(path).read_text())
    blockers = policy.get("qualification_blockers")
    if not isinstance(blockers, list) or blockers:
        raise RuntimeError("CI lifecycle rollout blocked: " + str(blockers))
    if not policy.get("workflows"):
        raise RuntimeError("CI lifecycle rollout blocked: no required workflows")


if __name__ == "__main__":
    check(sys.argv[1])
