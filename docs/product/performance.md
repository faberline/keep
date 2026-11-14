# keep performance

## Host class

The nightly gate runs on the host class `gha-ubuntu-latest` (GitHub `ubuntu-latest`, 4 CPUs, 1-core cells only, `KEEP_PERF_CORES=1`). The baseline was recorded by `keep-perf-gate` run 43578822818 (workflow_dispatch, `record=1`), merged by PR #121. Peers: redis 8.4, dragonfly, keydb, garnet. Their image digests are in `crates/keep/tests/perf-baseline.json` under the key `images`.

## Throughput

Operations per second on 1 core, from the log of run 43578822818.

| Tier | Target | SET/s | GET/s | CAS/s |
|---|---|---|---|---|
| ram | keep | 11567 | 15589 | 12025 |
| ram | redis | 65580 | 66410 | 53746 |
| ram | dragonfly | 53785 | 54506 | 27430 |
| ram | keydb | 35925 | 36002 | 32226 |
| ram | garnet | 31496 | 32290 | 25165 |
| tiered | keep | 11566 | 15309 | 11905 |
| tiered | redis | 42421 | 65358 | 36821 |
| tiered | keydb | 37005 | 36028 | 31110 |
| tiered | garnet | 18239 | 37870 | 15972 |

These absolute numbers come from the log of the run above. `perf-baseline.json` stores only ratios, so the ratio table below is what the gate enforces.

## Latency and resource use

Absolute p50, absolute p99 and server resource use (CPU, memory) are not recorded. The gate records p99 only as a ratio (peer p99 divided by keep p99, shown as the `p99` cells below). Do not read a number into these fields. A follow-up issue must add them to the gate.

## Regression threshold

- `throughput_ratchet`: 0.85
- `p99_ratchet`: 0.7
- `max_cv`: 0.1
- `ops_min_ratio_factor`: 0.8
- `p99_min_ratio_factor`: 0.6

A cell passes when its measured ratio is at least the larger of its `min_ratio` and the ratcheted baseline. A cell with `gate: ratchet` fails the nightly job on a regression; a cell with `gate: report` is only reported. A trial spread above `max_cv` is `UNSTABLE` and must be rerun. The ratchet maths is tested by `judge_uses_max_of_min_ratio_and_ratcheted_baseline` and `evaluate_fails_closed` in `crates/keep/tests/perf_gate_vs_kv.rs`. A ratchet is never lowered; `KEEP_PERF_RECORD=1` only raises a baseline.

## Baseline ratios

Ratio is keep divided by the peer for `ops` cells, and the peer divided by keep for `p99` cells, so higher is better for keep.

| Cell | Gate | Baseline | Min ratio |
|---|---|---|---|
| `ram/1c/CAS/ops/dragonfly` | ratchet | 0.4383 | 0.3506 |
| `ram/1c/CAS/ops/garnet` | report | 0.4778 | 0.3822 |
| `ram/1c/CAS/ops/keydb` | report | 0.3731 | 0.2984 |
| `ram/1c/CAS/ops/redis` | ratchet | 0.2237 | 0.1789 |
| `ram/1c/CAS/p99/dragonfly` | ratchet | 0.4062 | 0.2437 |
| `ram/1c/CAS/p99/garnet` | report | 4.9406 | 2.9643 |
| `ram/1c/CAS/p99/keydb` | report | 0.9453 | 0.5671 |
| `ram/1c/CAS/p99/redis` | ratchet | 0.2755 | 0.1653 |
| `ram/1c/GET/ops/dragonfly` | ratchet | 0.286 | 0.2288 |
| `ram/1c/GET/ops/garnet` | report | 0.4827 | 0.3861 |
| `ram/1c/GET/ops/keydb` | report | 0.433 | 0.3464 |
| `ram/1c/GET/ops/redis` | ratchet | 0.2347 | 0.1877 |
| `ram/1c/GET/p99/dragonfly` | ratchet | 0.2765 | 0.1659 |
| `ram/1c/GET/p99/garnet` | report | 9.6195 | 5.7717 |
| `ram/1c/GET/p99/keydb` | report | 1.2614 | 0.7568 |
| `ram/1c/GET/p99/redis` | ratchet | 0.2796 | 0.1677 |
| `ram/1c/SET/ops/dragonfly` | ratchet | 0.215 | 0.172 |
| `ram/1c/SET/ops/garnet` | report | 0.3672 | 0.2937 |
| `ram/1c/SET/ops/keydb` | report | 0.3219 | 0.2575 |
| `ram/1c/SET/ops/redis` | ratchet | 0.1763 | 0.141 |
| `ram/1c/SET/p99/dragonfly` | ratchet | 0.2016 | 0.1209 |
| `ram/1c/SET/p99/garnet` | report | 8.1003 | 4.8601 |
| `ram/1c/SET/p99/keydb` | report | 0.8417 | 0.505 |
| `ram/1c/SET/p99/redis` | ratchet | 0.2238 | 0.1342 |
| `tiered/1c/CAS/ops/garnet` | report | 0.7453 | 0.5962 |
| `tiered/1c/CAS/ops/keydb` | report | 0.3826 | 0.306 |
| `tiered/1c/CAS/ops/redis` | ratchet | 0.3233 | 0.2586 |
| `tiered/1c/CAS/p99/garnet` | report | 1.8133 | 1.0879 |
| `tiered/1c/CAS/p99/keydb` | report | 0.5988 | 0.3592 |
| `tiered/1c/CAS/p99/redis` | ratchet | 0.3738 | 0.2242 |
| `tiered/1c/GET/ops/garnet` | report | 0.4042 | 0.3233 |
| `tiered/1c/GET/ops/keydb` | report | 0.4249 | 0.3399 |
| `tiered/1c/GET/ops/redis` | ratchet | 0.2342 | 0.1873 |
| `tiered/1c/GET/p99/garnet` | report | 0.7327 | 0.4396 |
| `tiered/1c/GET/p99/keydb` | report | 1.1135 | 0.6681 |
| `tiered/1c/GET/p99/redis` | ratchet | 0.2937 | 0.1762 |
| `tiered/1c/SET/ops/garnet` | report | 0.6341 | 0.5072 |
| `tiered/1c/SET/ops/keydb` | report | 0.3125 | 0.25 |
| `tiered/1c/SET/ops/redis` | ratchet | 0.2726 | 0.218 |
| `tiered/1c/SET/p99/garnet` | report | 0.8576 | 0.5145 |
| `tiered/1c/SET/p99/keydb` | report | 0.5517 | 0.331 |
| `tiered/1c/SET/p99/redis` | ratchet | 0.3827 | 0.2296 |
