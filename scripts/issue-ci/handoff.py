#!/usr/bin/env python3
"""Trusted CI controller. Reads GitHub metadata, never executes PR code/artifacts."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
from urllib.parse import quote


def api(path, method='GET', data=None):
    command = ['gh', 'api', '--method', method, path]
    kwargs = dict(text=True)
    if data is not None:
        command += ['--input', '-']
        kwargs['input'] = json.dumps(data)
    output = subprocess.check_output(command, **kwargs)
    return json.loads(output) if output.strip() else None


def pages(path, key=None):
    items = []
    for page in range(1, 1001):
        value = api(f'{path}{"&" if "?" in path else "?"}per_page=100&page={page}')
        batch = value[key] if key else value
        items.extend(batch)
        if len(batch) < 100:
            return items
    raise RuntimeError('pagination limit reached; refuse partial evidence')


def load_config(path):
    config = json.loads(Path(path).read_text())
    workflows = config['workflows']
    if not workflows or len({w['path'] for w in workflows}) != len(workflows):
        raise RuntimeError('required workflow paths must be nonempty and unique')
    for w in workflows:
        if not re.fullmatch(r'\.github/workflows/[\w.-]+\.ya?ml', w['path']) or not w['jobs'] or not all(isinstance(j, str) and j.strip() for j in w['jobs']) or len(set(w['jobs'])) != len(w['jobs']):
            raise RuntimeError('each workflow needs an explicit nonempty unique job-name list')
    return config


def evaluate(workflows, snapshots):
    """Aggregate latest runs of ALL configured workflows; no vacuous all-green."""
    pending, failures, evidence = [], [], []
    for workflow in workflows:
        path = workflow['path']
        snapshot = snapshots.get(path)
        if not snapshot:
            pending.append(f'{path}: missing run')
            continue
        run, jobs = snapshot
        evidence.append(f'{run["id"]}:{run.get("run_attempt", 1)}')
        if run['status'] != 'completed':
            pending.append(f'{path}: {run["status"]}')
        elif run.get('conclusion') in ('failure', 'timed_out', 'action_required', 'startup_failure'):
            failures.append(f'{path}: {run["conclusion"]} {run["html_url"]}')
        elif run.get('conclusion') != 'success':
            pending.append(f'{path}: {run.get("conclusion")} {run["html_url"]}')
        for name in workflow['jobs']:
            matching = [j for j in jobs if j['name'] == name]
            if not matching:
                pending.append(f'{path} / {name}: missing job')
            for job in matching:
                link = job['html_url']
                if job['status'] != 'completed':
                    pending.append(f'{path} / {name}: {job["status"]} {link}')
                elif job.get('conclusion') in ('failure', 'timed_out', 'action_required', 'startup_failure'):
                    failures.append(f'{path} / {name}: {job["conclusion"]} {link}')
                elif job.get('conclusion') != 'success':
                    # Skips need a separate future policy; never assume justification.
                    pending.append(f'{path} / {name}: {job.get("conclusion")} {link}')
    state = 'todo' if failures else 'pending' if pending else 'done'
    return state, failures + pending, sorted(evidence)


def snapshot(repo, sha, config):
    root = f'repos/{repo}'
    snapshots = {}
    for workflow in config['workflows']:
        runs = pages(f'{root}/actions/workflows/{quote(workflow["path"].split("/")[-1], safe="")}/runs?head_sha={sha}&event=pull_request', 'workflow_runs')
        runs = [r for r in runs if r['head_sha'] == sha and r.get('event') == 'pull_request' and r.get('head_repository', {}).get('full_name') == repo and r.get('path') == workflow['path']]
        if not runs:
            continue
        latest = max(runs, key=lambda r: (r['run_number'], r.get('run_attempt', 1)))
        # Run-attempt endpoint prevents jobs from a previous attempt filling holes.
        jobs = pages(f'{root}/actions/runs/{latest["id"]}/attempts/{latest.get("run_attempt", 1)}/jobs', 'jobs')
        snapshots[workflow['path']] = latest, jobs
    return snapshots


def process_head(args, config, check_event=True):
    root = f'repos/{args.repo}'
    pr_path = f'{root}/pulls/{args.pr}'
    pr = api(pr_path)
    if pr['state'] != 'open' or pr['head']['sha'] != args.head or pr['head']['repo']['full_name'] != args.repo:
        raise RuntimeError('closed, foreign, or stale PR head; no mutation')
    match = re.fullmatch(r'(?:issue/|agy/issue-)([0-9]+)', pr['head']['ref'])
    if not match:
        raise RuntimeError('PR branch has no supported issue association')
    issue = int(match[1])
    if not re.search(rf'(?im)^Closes #{issue}\s*$', pr.get('body') or ''):
        raise RuntimeError('PR body must explicitly link its branch issue with Closes #N')
    snapshots = snapshot(args.repo, args.head, config)
    state, reasons, evidence = evaluate(config['workflows'], snapshots)
    print(json.dumps(dict(state=state, head=args.head, evidence=evidence, reasons=reasons)))
    if not args.apply:
        return 0 if state == 'done' else 2
    if check_event:
        if args.run_id is None:
            raise RuntimeError('--apply requires workflow_run event --run-id')
        event_run = api(f'{root}/actions/runs/{args.run_id}')
        if event_run['head_sha'] != args.head or event_run.get('event') != 'pull_request' or str(event_run['id']) not in {e.split(':')[0] for e in evidence}:
            raise RuntimeError('stale/unconfigured triggering run; no mutation')
    if state == 'pending':
        return 0
    issue_path = f'{root}/issues/{issue}'
    current = api(issue_path)
    labels = {l['name'] for l in current['labels']}
    if current['state'] != 'open' or not labels & {'todo', 'doing', 'done'} or 'blocked' in labels:
        raise RuntimeError('issue is closed, blocked, or unmanaged; no mutation')
    comments = pages(f'{issue_path}/comments')
    policy = hashlib.sha256(json.dumps(config, sort_keys=True).encode()).hexdigest()
    # A failure hands a head back once per coding epoch, regardless of how
    # many more workflows fail while AGY is fixing it. A successfully APPLIED
    # done receipt begins a new epoch, allowing a later done -> todo regression.
    qualified = [c for c in comments
                 if c['user']['login'] == 'github-actions[bot]'
                 and c['body'].startswith('<!-- issue-ci:')
                 and c['body'].splitlines()[0].endswith(':applied -->')
                 and f'\nCI: done for PR #{args.pr} head `{args.head}`.\n' in c['body']
                 and f'\nPolicy: {policy}\n' in c['body']]
    epoch = qualified[-1]['id'] if qualified else 0
    receipt_key = evidence if state == 'done' else ['coding-epoch', epoch]
    digest = hashlib.sha256(json.dumps([args.head, receipt_key, config, state], sort_keys=True).encode()).hexdigest()
    prefix = f'<!-- issue-ci:{digest}:'
    journals = [c for c in comments if c['body'].startswith(prefix) and c['user']['login'] == 'github-actions[bot]']
    journal = journals[-1] if journals else None
    phase = journal['body'].split(prefix, 1)[1].split(' -->', 1)[0] if journal else 'pending'
    if phase == 'applied':
        print('duplicate applied head/run handoff; no mutation')
        return 0
    if phase not in ('pending', 'ready'):
        raise RuntimeError('invalid handoff journal phase')
    detail = '\n'.join(reasons) if reasons else 'All explicitly required workflow/job gates passed.'
    urls = '\n'.join(s[0]['html_url'] for s in snapshots.values())

    def body(phase):
        return f'{prefix}{phase} -->\nCI: {state} for PR #{args.pr} head `{args.head}`.\nPolicy: {policy}\n{detail}\n{urls}'

    def fresh_labels():
        fresh = api(pr_path)
        if fresh['state'] != 'open' or fresh['head']['sha'] != args.head:
            raise RuntimeError('PR moved during handoff; no further label mutation')
        current = api(issue_path)
        labels = {l['name'] for l in current['labels']}
        if current['state'] != 'open' or 'blocked' in labels or not labels & {'todo', 'doing', 'done'}:
            raise RuntimeError('issue lifecycle changed during handoff')
        return labels

    labels = fresh_labels()
    if journal is None:
        phase = 'ready' if state in labels else 'pending'
        journal = api(f'{issue_path}/comments', 'POST', {'body': body(phase)})
    comment_path = f'{root}/issues/comments/{journal["id"]}'
    # A pending intent never suppresses a retry after head/snapshot/API failures.
    labels = fresh_labels()
    if snapshot(args.repo, args.head, config) != snapshots:
        raise RuntimeError('CI runs changed during evaluation; pending intent is retryable')
    if phase == 'pending':
        if state not in labels:
            api(f'{issue_path}/labels', 'POST', {'labels': [state]})
        # Durable proof that the desired label was added BEFORE deleting old
        # labels. Workers refuse mixed states, so they cannot claim between
        # the add and this checkpoint, even if the controller crashes.
        api(comment_path, 'PATCH', {'body': body('ready')})
    labels = fresh_labels()
    if state in labels:
        for old in sorted((labels & {'todo', 'doing', 'done'}) - {state}):
            live = fresh_labels()
            if state not in live:
                break
            if old in live:
                api(f'{issue_path}/labels/{old}', 'DELETE')
    # ready + desired label absent means a later actor already consumed it
    # (e.g. AGY claimed todo -> doing). Never add it again or undo that claim.
    api(comment_path, 'PATCH', {'body': body('applied')})
    return 0


def reconcile(args, config):
    """Every wake scans all managed open PRs: a replaced pending wake loses none."""
    root = f'repos/{args.repo}'
    event = api(f'{root}/actions/runs/{args.run_id}')
    if (event.get('event') != 'pull_request'
            or event.get('head_repository', {}).get('full_name') != args.repo
            or event.get('path') not in {w['path'] for w in config['workflows']}):
        raise RuntimeError('unconfigured/foreign reconciliation wake')
    errors = []
    for pr in pages(f'{root}/pulls?state=open'):
        match = re.fullmatch(r'(?:issue/|agy/issue-)([0-9]+)', pr['head']['ref'])
        if (not match or (pr['head'].get('repo') or {}).get('full_name') != args.repo
                or not re.search(rf'(?im)^Closes #{int(match[1])}\s*$', pr.get('body') or '')):
            continue
        current = argparse.Namespace(**vars(args))
        current.pr, current.head, current.apply = pr['number'], pr['head']['sha'], True
        try:
            # Event metadata only wakes us. All decisions use fresh current-head
            # snapshots, including PRs unrelated to the triggering run.
            process_head(current, config, check_event=False)
        except (RuntimeError, subprocess.CalledProcessError, OSError, ValueError, KeyError, TypeError) as exc:
            errors.append(f'PR #{current.pr}: {exc}')
    if errors:
        raise RuntimeError('; '.join(errors))
    return 0


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--repo', required=True)
    parser.add_argument('--config', required=True, help='trusted default-branch config, never from PR')
    parser.add_argument('--pr', type=int)
    parser.add_argument('--head')
    parser.add_argument('--run-id', type=int)
    parser.add_argument('--apply', action='store_true')
    parser.add_argument('--reconcile', action='store_true', help='trusted wake: reconcile every managed current PR head')
    args = parser.parse_args()
    if not re.fullmatch(r'[\w.-]+/[\w.-]+', args.repo):
        raise RuntimeError('invalid repository')
    config = load_config(args.config)
    if args.reconcile:
        if args.run_id is None or args.pr is not None or args.head is not None:
            raise RuntimeError('--reconcile requires --run-id and scans PRs itself')
        return reconcile(args, config)
    if args.pr is None or not re.fullmatch('[0-9a-f]{40}', args.head or ''):
        raise RuntimeError('--pr and valid --head required')
    return process_head(args, config)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (RuntimeError, subprocess.CalledProcessError, OSError, ValueError, KeyError, TypeError) as exc:
        print(f'refused: {exc}', file=sys.stderr)
        sys.exit(2)
