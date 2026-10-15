#!/usr/bin/env bash
# Test command for the shared GKE test cluster (faberline/aw
# .github/workflows/gke-test.yml). The run calls it from the repository root
# after the keep StatefulSet is rolled out, with NAMESPACE, EVIDENCE_DIR,
# RUN_LABEL, and KUBECONFIG set.
#
# Three steps, each leaves evidence in EVIDENCE_DIR:
#   1. /readyz answers 200 through a port-forward.
#   2. PUT /kv/{key} then GET /kv/{key} returns the same value.
#   3. keep-0 is deleted and comes back Ready; the same GET still returns the
#      value. This proves the PVC keeps the data.
set -euo pipefail

: "${NAMESPACE:?}" "${EVIDENCE_DIR:?}" "${RUN_LABEL:?}"

ns="$NAMESPACE"
pod="keep-0"
local_port=17117
base_url="http://127.0.0.1:${local_port}"
key="rt-${RUN_LABEL}"
forward_pid=""

mkdir -p "$EVIDENCE_DIR"

stop_forward() {
  if [[ -n "$forward_pid" ]]; then
    kill "$forward_pid" >/dev/null 2>&1 || true
    wait "$forward_pid" >/dev/null 2>&1 || true
    forward_pid=""
  fi
}
trap stop_forward EXIT

# A port-forward pins one pod. Step 3 replaces that pod, so a dead forward is
# started again instead of treated as a failure.
start_forward() {
  stop_forward
  local deadline=$((SECONDS + 120))
  while ((SECONDS < deadline)); do
    if [[ -z "$forward_pid" ]] || ! kill -0 "$forward_pid" >/dev/null 2>&1; then
      kubectl -n "$ns" port-forward service/keep "${local_port}:7117" \
        >>"$EVIDENCE_DIR/port-forward.log" 2>&1 &
      forward_pid="$!"
      sleep 1
    fi
    if curl --max-time 5 --silent --fail "$base_url/readyz" >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
  done
  echo "keep: /readyz did not answer through the port-forward in 120s" >&2
  return 1
}

# http_json <evidence-file> <expected-status> <method> <path> [body]
http_json() {
  local out="$1" want="$2" method="$3" path="$4" body="${5:-}"
  local args=(--max-time 15 --silent --show-error -o "$EVIDENCE_DIR/$out"
    -w '%{http_code}' -X "$method" "$base_url$path")
  [[ -n "$body" ]] && args+=(-H 'content-type: application/json' --data "$body")
  local status
  status="$(curl "${args[@]}")"
  if [[ "$status" != "$want" ]]; then
    echo "keep: $method $path returned $status, expected $want" >&2
    cat "$EVIDENCE_DIR/$out" >&2 || true
    return 1
  fi
}

read_back() {
  local out="$1"
  http_json "$out" 200 GET "/kv/$key"
  jq -e --arg run "$RUN_LABEL" '.value.run == $run and .value.n == 1' \
    "$EVIDENCE_DIR/$out" >/dev/null || {
    echo "keep: read-back value for $key is wrong" >&2
    cat "$EVIDENCE_DIR/$out" >&2
    return 1
  }
}

echo "[keep] step 1: readyz" >&2
start_forward
curl --max-time 5 --silent "$base_url/readyz" >"$EVIDENCE_DIR/step1-readyz.txt" || true

echo "[keep] step 2: PUT and GET round trip" >&2
http_json step2-write.json 200 PUT "/kv/$key" "{\"value\":{\"n\":1,\"run\":\"$RUN_LABEL\"}}"
read_back step2-read.json

echo "[keep] step 3: delete $pod; the data must survive" >&2
kubectl -n "$ns" delete pod "$pod" --wait=true --timeout=180s >&2
# The StatefulSet makes the new pod a moment after the old one is gone, and
# `kubectl wait` on a pod that does not exist fails at once. Wait for it first.
deadline=$((SECONDS + 120))
until kubectl -n "$ns" get pod "$pod" >/dev/null 2>&1; do
  if ((SECONDS >= deadline)); then
    echo "keep: the StatefulSet did not make $pod again in 120s" >&2
    exit 1
  fi
  sleep 2
done
kubectl -n "$ns" wait --for=condition=Ready "pod/$pod" --timeout=300s >&2
start_forward
read_back step3-read-after-restart.json

echo "[keep] PASS: readyz, round trip, data survives a pod restart" |
  tee "$EVIDENCE_DIR/verdict.txt" >&2
