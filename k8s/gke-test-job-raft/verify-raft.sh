#!/usr/bin/env bash
# Runs inside the keep-test Job on pool-keep-client, raft topology: the keep
# pods form one raft group (shard 0). The Job's logs are the evidence; the
# shared run copies them into its artifact.
#
# Six steps. Each prints a line that starts with `[keep] step N:`:
#   1. /readyz answers 200 on every pod (per-pod DNS, not the client Service).
#   2. Exactly one pod of shard 0 reports is_leader: true.
#   3. PUT a key on the leader.
#   4. Every pod reads the key back with the value of this run.
#   5. The leader pod is deleted. One of the other pods becomes leader, and
#      the deleted pod comes back Ready.
#   6. Every pod reads the key back again: the write survived the failover.
# The last line is the pass line. It prints only when every step passed.
set -euo pipefail

: "${NAMESPACE:?}" "${POD_NAME:?}" "${REPLICAS:?}"

pods=()
for ((i = 0; i < REPLICAS; i++)); do
  pods+=("keep-$i")
done
key="rt-raft-${POD_NAME}"
body="$(mktemp)"
leader=""

pod_url() {
  echo "http://$1.keep-headless.$NAMESPACE.svc:7117"
}

# status_of <base-url> <method> <path> [body]: prints the HTTP status and
# leaves the response in $body. Prints 000 when the pod does not answer.
status_of() {
  local base="$1" method="$2" path="$3" data="${4:-}"
  local args=(--max-time 15 --silent --show-error -o "$body" -w '%{http_code}'
    -X "$method" "$base$path")
  if [[ -n "$data" ]]; then
    args+=(-H 'content-type: application/json' --data "$data")
  fi
  curl "${args[@]}" 2>/dev/null || true
}

# http_json <base-url> <expected-status> <method> <path> [body]
http_json() {
  local base="$1" want="$2" method="$3" path="$4" data="${5:-}"
  local status
  status="$(status_of "$base" "$method" "$path" "$data")"
  echo "keep: $method $path -> $status $(cat "$body")"
  if [[ "$status" != "$want" ]]; then
    echo "keep: expected $want from $base$path" >&2
    exit 1
  fi
}

# wait_until <seconds> <what> <command...>: retries the command every 2 s and
# exits 1 with a message when it does not succeed in time.
wait_until() {
  local limit="$1" what="$2"
  shift 2
  local deadline=$((SECONDS + limit))
  until "$@"; do
    if ((SECONDS >= deadline)); then
      echo "keep: $what did not hold in ${limit}s" >&2
      exit 1
    fi
    sleep 2
  done
}

ready_ok() {
  [[ "$(status_of "$1" GET /readyz)" == 200 ]]
}

all_ready() {
  local pod
  for pod in "${pods[@]}"; do
    ready_ok "$(pod_url "$pod")" || return 1
  done
}

# find_leader <pod>...: sets $leader and succeeds when exactly one of the
# given pods answers /shard/0/raftz with is_leader true.
find_leader() {
  local pod found="" count=0
  for pod in "$@"; do
    if [[ "$(status_of "$(pod_url "$pod")" GET /shard/0/raftz)" == 200 ]] &&
      jq -e '.is_leader == true' "$body" >/dev/null 2>&1; then
      found="$pod"
      count=$((count + 1))
    fi
  done
  if ((count == 1)); then
    leader="$found"
    return 0
  fi
  return 1
}

# read_back <pod>: the key reads back with the value that this run wrote.
read_back() {
  [[ "$(status_of "$(pod_url "$1")" GET "/kv/$key")" == 200 ]] || return 1
  jq -e --arg run "$POD_NAME" '.value.run == $run and .value.n == 1' "$body" >/dev/null 2>&1
}

pod_exists() {
  kubectl -n "$NAMESPACE" get pod "$1" >/dev/null 2>&1
}

# check_reads: every pod returns the value that this run wrote.
check_reads() {
  local pod
  for pod in "${pods[@]}"; do
    wait_until 60 "read of $key on $pod" read_back "$pod"
    echo "keep: $pod returns $key with the value of this run"
  done
}

echo "[keep] step 1: readyz on every pod"
wait_until 600 "/readyz on all pods" all_ready

echo "[keep] step 2: one leader of shard 0"
wait_until 120 "a single leader of shard 0" find_leader "${pods[@]}"
echo "keep: leader is $leader"

echo "[keep] step 3: PUT $key on the leader $leader"
http_json "$(pod_url "$leader")" 200 PUT "/kv/$key" "{\"value\":{\"n\":1,\"run\":\"$POD_NAME\"}}"

echo "[keep] step 4: every pod reads the value"
check_reads

echo "[keep] step 5: delete the leader $leader; a new leader must come up"
old="$leader"
kubectl -n "$NAMESPACE" delete pod "$old" --wait=true --timeout=180s
others=()
for pod in "${pods[@]}"; do
  if [[ "$pod" != "$old" ]]; then
    others+=("$pod")
  fi
done
wait_until 120 "a new leader among ${others[*]}" find_leader "${others[@]}"
echo "keep: new leader is $leader"
wait_until 120 "$old is created again" pod_exists "$old"
kubectl -n "$NAMESPACE" wait --for=condition=Ready "pod/$old" --timeout=300s
wait_until 120 "/readyz on $old" ready_ok "$(pod_url "$old")"

echo "[keep] step 6: every pod reads the value after the failover"
check_reads

echo "[keep] PASS: raft 3 replicas + leader failover + durability"
