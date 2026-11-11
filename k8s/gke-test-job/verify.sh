#!/usr/bin/env bash
# Runs inside the keep-test Job on pool-keep-client. The Job's logs are the
# evidence; the shared run copies them into its artifact.
#
# Three steps:
#   1. /readyz answers 200 through the keep Service.
#   2. PUT /kv/{key} then GET /kv/{key} returns the same value.
#   3. keep-0 is deleted and comes back Ready; the same GET still returns the
#      value. This proves the PVC keeps the data.
set -euo pipefail

: "${NAMESPACE:?}" "${POD_NAME:?}" "${KEEP_URL:?}"

pod="keep-0"
key="rt-${POD_NAME}"
body="$(mktemp)"

wait_ready() {
  local deadline=$((SECONDS + 300))
  until curl --max-time 5 --silent --fail "$KEEP_URL/readyz" >/dev/null 2>&1; do
    if ((SECONDS >= deadline)); then
      echo "keep: /readyz did not answer through the Service in 300s" >&2
      return 1
    fi
    sleep 2
  done
}

# http_json <expected-status> <method> <path> [body]; the response is in $body.
http_json() {
  local want="$1" method="$2" path="$3" data="${4:-}"
  local args=(--max-time 15 --silent --show-error -o "$body" -w '%{http_code}'
    -X "$method" "$KEEP_URL$path")
  [[ -n "$data" ]] && args+=(-H 'content-type: application/json' --data "$data")
  local status
  status="$(curl "${args[@]}")"
  echo "keep: $method $path -> $status $(cat "$body")"
  if [[ "$status" != "$want" ]]; then
    echo "keep: expected $want" >&2
    return 1
  fi
}

read_back() {
  http_json 200 GET "/kv/$key"
  jq -e --arg run "$POD_NAME" '.value.run == $run and .value.n == 1' "$body" >/dev/null || {
    echo "keep: read-back value for $key is wrong" >&2
    return 1
  }
}

echo "[keep] step 1: readyz"
wait_ready

echo "[keep] step 2: PUT and GET round trip"
http_json 200 PUT "/kv/$key" "{\"value\":{\"n\":1,\"run\":\"$POD_NAME\"}}"
read_back

echo "[keep] step 3: delete $pod; the data must survive"
kubectl -n "$NAMESPACE" delete pod "$pod" --wait=true --timeout=180s
# The StatefulSet makes the new pod a moment after the old one is gone, and
# `kubectl wait` on a pod that does not exist fails at once. Wait for it first.
deadline=$((SECONDS + 120))
until kubectl -n "$NAMESPACE" get pod "$pod" >/dev/null 2>&1; do
  if ((SECONDS >= deadline)); then
    echo "keep: the StatefulSet did not make $pod again in 120s" >&2
    exit 1
  fi
  sleep 2
done
kubectl -n "$NAMESPACE" wait --for=condition=Ready "pod/$pod" --timeout=300s
wait_ready
read_back

echo "[keep] PASS: readyz, round trip, data survives a pod restart"
