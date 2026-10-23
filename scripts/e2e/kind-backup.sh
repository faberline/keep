#!/usr/bin/env bash
# kind e2e for #5: the committed CRD + the operator's backup CronJob, applied to
# a real API server.
#
#   1. build a `keep` image with the operator (`CARGO_FEATURES=operator`) and
#      load it into a throwaway kind cluster;
#   2. apply k8s/operator (CRD + RBAC + controller) with that image;
#   3. apply a dev-profile Keep CR carrying `spec.backup` and wait for the store;
#   4. assert the API server kept `spec.backup` (a field missing from the CRD
#      schema is pruned silently) and the operator rendered the CronJob;
#   5. write a key, trigger the CronJob by hand, and require the job to finish
#      on pod 0's node with a snapshot that is larger than an empty one.
#
# Usage: scripts/e2e/kind-backup.sh
#   KIND_CLUSTER   cluster name (default keep-backup-e2e; created if missing)
#   KEEP_E2E_IMAGE image tag to build/load (default keep:e2e-operator)
#   CARGO_PROFILE  debug (default, faster build) or release
#   SKIP_BUILD=1   reuse an already-built KEEP_E2E_IMAGE
#   KEEP_CLUSTER=1 leave the kind cluster running afterwards
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
CLUSTER=${KIND_CLUSTER:-keep-backup-e2e}
IMAGE=${KEEP_E2E_IMAGE:-keep:e2e-operator}
PROFILE=${CARGO_PROFILE:-debug}
NS=keep-e2e
NAME=store
CTX="kind-$CLUSTER"
KC=(kubectl --context "$CTX")

log() { printf '\n==> %s\n' "$*"; }
fail() {
  printf '\nFAIL: %s\n' "$*" >&2
  "${KC[@]}" get keep,sts,pod,cronjob,job -n "$NS" -o wide >&2 || true
  "${KC[@]}" logs -n keep-system deploy/keep-operator --tail=50 >&2 || true
  exit 1
}

created=0
cleanup() {
  if [[ $created == 1 && ${KEEP_CLUSTER:-0} != 1 ]]; then
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

if [[ ${SKIP_BUILD:-0} != 1 ]]; then
  log "build $IMAGE (profile=$PROFILE, features=operator)"
  docker build -f "$ROOT/Dockerfile" \
    --build-arg CARGO_PROFILE="$PROFILE" --build-arg CARGO_FEATURES=operator \
    -t "$IMAGE" "$ROOT"
fi

if ! kind get clusters | grep -qx "$CLUSTER"; then
  log "create kind cluster $CLUSTER"
  kind create cluster --name "$CLUSTER" --wait 120s
  created=1
fi
log "load $IMAGE"
kind load docker-image "$IMAGE" --name "$CLUSTER"

log "apply operator (CRD + RBAC + controller)"
"${KC[@]}" kustomize "$ROOT/k8s/operator" \
  | sed -E "s#^( *image: )keep:.*#\1$IMAGE#" \
  | "${KC[@]}" apply -f -
"${KC[@]}" wait --for=condition=Established crd/keeps.keep.dev --timeout=60s
"${KC[@]}" -n keep-system rollout status deploy/keep-operator --timeout=180s \
  || fail "operator did not become ready"

log "apply Keep CR with spec.backup"
"${KC[@]}" create namespace "$NS" --dry-run=client -o yaml | "${KC[@]}" apply -f -
"${KC[@]}" apply -f - <<EOF
apiVersion: keep.dev/v1alpha1
kind: Keep
metadata:
  name: $NAME
  namespace: $NS
spec:
  image: $IMAGE
  imagePullPolicy: Never
  shardCount: 1
  replicasPerShard: 1
  voterCount: 1
  engineShards: 16
  storage: 1Gi
  resources:
    cpu: "250m"
    memory: 256Mi
  backup:
    schedule: "0 3 * * *"
    destination: file:///tmp/keep-backups
    retentionSecs: 86400
EOF

log "spec.backup survives the API server's structural-schema pruning"
dest=$("${KC[@]}" get keep "$NAME" -n "$NS" -o jsonpath='{.spec.backup.destination}')
[[ $dest == file:///tmp/keep-backups ]] || fail "spec.backup was pruned (got destination='$dest')"

log "operator renders the store and the backup CronJob"
for _ in $(seq 1 90); do
  "${KC[@]}" get cronjob "$NAME-backup" -n "$NS" >/dev/null 2>&1 && break
  sleep 2
done
"${KC[@]}" get cronjob "$NAME-backup" -n "$NS" >/dev/null 2>&1 || fail "CronJob $NAME-backup never appeared"
for _ in $(seq 1 90); do
  "${KC[@]}" get pod "$NAME-0" -n "$NS" >/dev/null 2>&1 && break
  sleep 2
done
"${KC[@]}" wait --for=condition=Ready pod/"$NAME-0" -n "$NS" --timeout=300s \
  || fail "store pod $NAME-0 not ready"

log "write a key through the store"
"${KC[@]}" -n "$NS" port-forward "pod/$NAME-0" 17117:7117 >/dev/null 2>&1 &
pf=$!
for _ in $(seq 1 50); do curl -sf http://127.0.0.1:17117/healthz >/dev/null && break; sleep 0.2; done
curl -sf -XPUT http://127.0.0.1:17117/kv/e2e-backup -H 'content-type: application/json' \
  -d '{"value":"persisted-before-backup"}' >/dev/null || { kill $pf; fail "PUT failed"; }
kill $pf; wait $pf 2>/dev/null || true
# The WAL is fsynced on an interval; give it a beat before the job reads it.
sleep 2

log "trigger the CronJob by hand"
job="$NAME-backup-manual-$(date +%s)"
"${KC[@]}" create job "$job" --from=cronjob/"$NAME-backup" -n "$NS"
"${KC[@]}" wait --for=condition=Complete job/"$job" -n "$NS" --timeout=300s \
  || { "${KC[@]}" logs -n "$NS" job/"$job" >&2 || true; fail "backup job did not complete"; }
out=$("${KC[@]}" logs -n "$NS" job/"$job")
printf '%s\n' "$out"
bytes=$(printf '%s\n' "$out" | sed -nE 's/.*"bytes": *([0-9]+).*/\1/p' | head -1)
# An empty snapshot is `{"version":1,"values":[]}` (25 bytes); the key written
# above adds ~50. Anything at or below 25 means the job read an empty /data.
[[ -n $bytes && $bytes -gt 25 ]] || fail "backup snapshot is empty (${bytes:-?} bytes): /data not mounted?"
node_job=$("${KC[@]}" get pod -n "$NS" -l job-name="$job" -o jsonpath='{.items[0].spec.nodeName}')
node_store=$("${KC[@]}" get pod "$NAME-0" -n "$NS" -o jsonpath='{.spec.nodeName}')
[[ $node_job == "$node_store" ]] || fail "backup job ran on $node_job, store pod on $node_store"

log "PASS: CRD keeps spec.backup; CronJob ran on $node_store and wrote $bytes bytes"
