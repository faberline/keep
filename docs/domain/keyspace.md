# Keyspace

The keyspace context is the sharded multi-core KV engine `KvEngine` in
`crates/keep-keyspace`. It stores the keys and values, applies every operation,
and keeps the state that the WAL and snapshots persist.

## Value types

The `KvValue` variants in `crates/keep-shared-kernel/src/types.rs`:

- `Int`: 64-bit signed integer.
- `Float`: 64-bit floating point.
- `Decimal`: 128-bit fixed-point decimal for financial precision.
- `String`: UTF-8 string.
- `Bytes`: binary data.
- `List`: ordered list of values.
- `Map`: key-value map (Hash in Redis).
- `Set`: unordered set of unique strings.
- `SortedSet`: sorted set with scores (member to score).
- `Null`: null or none value.

## Key rule

`KvKey::new` in `crates/keep-shared-kernel/src/types.rs` accepts a key that is at
most 256 bytes. A longer key is rejected with `KeyTooLong`. An empty key is
rejected with `EmptyKey`.

## Operation groups

- Scalar: `/kv/{key}`, `/kv/{key}/incr`, `/cas`, `/setnx`.
- Batch: `/kv:mget`, `/kv:mset`, `/kv:mdel`.
- Lock: `/locks/{key}`.
- Hash: `/hashes/{key}`.
- List: `/lists/{key}`.
- Set: `/sets/{key}`.
- Sorted set: `/zsets/{key}`.
