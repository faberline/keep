# Competitor performance ratchet (keep vs Redis / Dragonfly / KeyDB / Garnet)

Issue #2. The gate is `crates/keep/tests/perf_gate_vs_kv.rs`
(`competitive_perf_gate`, `#[ignore]`, release-only) with its baseline in
`crates/keep/tests/perf-baseline.json`. It is a **ratchet**, not a
"must beat Redis" claim: thresholds come from keep's own measured ratios on a
given host class, and the gate fails when keep loses ground against a peer.

```bash
docker build -f Dockerfile.bench -t keep-bench:gate .
cargo test --release -p keep --test perf_gate_vs_kv competitive_perf_gate \
    -- --ignored --exact --nocapture --test-threads=1
# or: vat run perf-gate-competitors
```

## Matrix

| axis   | values |
|--------|--------|
| tier   | `ram` (no persistence) / `tiered` (durable before ack) |
| cores  | server pinned to 1 and to 4 CPUs (`--cpuset-cpus 0-(N-1)`) |
| phase  | `SET`, `GET`, `CAS` (pre-seeded keys; every CAS must swap) |
| metric | throughput (ops/s) and p99 latency |
| peers  | `redis`, `dragonfly` = **ratchet**; `keydb`, `garnet` = report-only |

keep's own 1 → 4 core throughput scaling is recorded as report-only
`{tier}/4c/{phase}/ops/keep-1c` cells.

Every cell is a ratio oriented so **higher is better for keep**:
throughput `keep / peer`, p99 `peer / keep`. Each target runs `KEEP_PERF_TRIALS`
(default 3) times; the cell ratio is median(keep) over median(peer).

## Thresholds

From `policy` in the baseline file:

| metric | ratchet | min_ratio at first record |
|--------|---------|---------------------------|
| throughput | fail when `ratio < max(min_ratio, 0.85 × baseline)` | `0.8 × measured` |
| p99 | fail when `ratio < max(min_ratio, 0.70 × baseline)` | `0.6 × measured` |

A failure where keep's own trial-to-trial CV exceeds `max_cv` (10%) is reported
as `UNSTABLE` (rerun first) rather than `REGRESSION`; both fail the run.

`KEEP_PERF_RECORD=1` re-records: baselines only move **up**, `min_ratio` never
moves. Lowering requires `KEEP_PERF_RECORD=force` plus `KEEP_PERF_NOTE`, which is
stored on the host entry so the reset is visible in review.

A host class may **demote** a Redis/Dragonfly cell to `report` when that cell
is not trustworthy there; the host entry must then carry a `note` saying why
(the baseline test enforces this). KeyDB/Garnet cells can never be promoted to
`ratchet`. `KEEP_PERF_RECORD` keeps each cell's existing gate.

## Fail-closed conditions

The run fails (not skips) when:

- the host class has no baseline (record one on that host first);
- a baseline ratchet cell was not measured (so subset runs via
  `KEEP_PERF_TIERS` / `KEEP_PERF_CORES` / `KEEP_PERF_TARGETS` are debug-only);
- keep's or a ratchet peer's image is missing, cannot be pulled, or is built
  for another CPU architecture (an emulated server would measure QEMU);
- a container does not start or never becomes ready;
- the server's storage mode does not match the tier: keep via
  `/info.persistence_enabled`, RESP servers via `CONFIG GET appendonly` /
  `appendfsync`; a durable tier that cannot be **proven** fails for gated targets;
- any request fails (non-2xx, RESP error, GET miss, CAS that did not swap);
- a ratio is not finite, or the baseline file is malformed.

Report-only peers that cannot run are printed as `UNAVAILABLE` and do not fail.

## Fairness, stated plainly

- **Protocol.** keep speaks HTTP/2 + JSON; the peers speak RESP over raw TCP.
  The ratio includes that cost on purpose: it is what a client pays.
- **Durability parity.** keep's `tiered` mode acks a write only after the group
  commit fsync (`/info.fsync_interval_ms` is the flush cadence of the idle
  case, not a delay before ack). Redis and KeyDB therefore run
  `--appendonly yes --appendfsync always`; Garnet runs `--aof
  --aof-commit-freq 0 --aof-commit-wait`. Dragonfly has no fsync-per-write AOF,
  so it has no `tiered` cells. In `ram` every server runs without persistence
  (`--save ""`, `--appendonly no`, `--dbfilename=`, `--disable-persistence`).
- **Threads.** Each server gets its native knob set to the core count: keep
  `--worker-threads`, Redis `--io-threads`, KeyDB `--server-threads`, Dragonfly
  `--proactor_threads`; Garnet sizes itself from the cpuset.
- **CAS.** keep uses its native `POST /kv/{key}/cas`; RESP peers use a single
  `EVALSHA` compare-and-set Lua script (one round trip, atomic). Garnet starts
  with `--lua`.
- **Isolation.** One server at a time, on a cpuset disjoint from the load
  generator (`bench_compare --format json` in a second container on the
  remaining CPUs), fresh anonymous data volume per server, `nofile=65536`.
  On macOS the containers share a private Docker bridge (bridge cost applies
  equally to every target); Linux CI uses `KEEP_PERF_NETWORK=host`.
- **Disk.** On OrbStack / Docker Desktop `fsync` may not reach physical media,
  so `tiered` numbers from laptops are indicative; the Linux nightly baseline
  is authoritative for the durable tier.
- **Images.** Peer images are pinned by digest in `perf-baseline.json`.
  Bumping a digest is a baseline change and normally needs a forced re-record.

## Host classes

Baselines are keyed by host class, derived from `docker info`
(`{os}-{arch}-{runtime}-{ncpu}c`, e.g. `darwin-aarch64-orbstack-10c`) or set
explicitly with `KEEP_PERF_HOST_CLASS` (CI pins `linux-x86_64-gha-4c`-style
names). The Linux nightly class is recorded by the first nightly run.

## First calibration — `darwin-aarch64-orbstack-10c` (2026-10-22)

Apple-silicon laptop under OrbStack, two raise-only rounds of 5 trials each
(`KEEP_PERF_RECORD=1`), concurrency 50, 64-byte values, 200k ops for RAM and
40k ops for tiered. Each cell shows its median keep/peer throughput ratio from
the second round:

| cell | Redis | Dragonfly | KeyDB | Garnet |
|------|------:|----------:|------:|-------:|
| ram 1c SET | 0.19 | 0.26 | 0.35 | 0.42 |
| ram 1c CAS | 0.28 | 0.42 | 0.41 | 0.54 |
| ram 4c SET | 0.38 | 0.37 | 0.47 | 0.37 |
| tiered 1c SET | 1.24 | — | 1.27 | 0.43 |
| tiered 1c CAS | 1.53 | — | 1.66 | 0.80 |
| tiered 4c SET | 0.47 | — | 10.8 | 0.91 |

Observations:

- **RAM.** keep's HTTP/2 + JSON path is about 2.4–5.3× slower than the RESP
  peers on one core, and about 2.1–2.7× slower on four.
- **Scaling.** keep scales about 1.7× from one core to four.
- **Durable tier.**
  - keep's group commit beats per-write `appendfsync always` at one core.
  - KeyDB's multi-threaded AOF collapses at four cores.
- **Noise.** The tiered numbers moved up to 2× between rounds. For example,
  Redis tiered 1c SET ran at 22.7k/s in one round and 9.9k/s in the other,
  because OrbStack virtualizes fsync. This host class therefore keeps the 24
  RAM Redis/Dragonfly cells as ratchets and demotes its tiered cells to
  `report`. The durable-tier ratchet belongs to the Linux nightly host class
  (`linux-x86_64-gha-4c`, 1-core cells; recorded by its first
  `workflow_dispatch` run with `record: "1"`).
- **Garnet.** Garnet's tiered mode cannot be verified over RESP, because it
  has no `appendfsync` setting. It stays report-only.
