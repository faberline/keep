# Keep quickstart

This page gets one Keep server running, sets and reads a key, and checks its
health. The `keep` commands on this page are checked against the real CLI help
by `crates/keep/tests/docs_lint.rs`. Docker, `curl` and `kubectl` lines are not
tested.

## Run the binary

Start the server on the default address with a local data directory:

```bash
keep --host 127.0.0.1 --port 7117 --data-dir ./data
```

The defaults are port `7117` and data directory `./data`. To run from source
instead:

```bash
cargo run -p keep --bin keep
```

## Run the image

Build the release image. The build argument selects the published release
(this is the form in the header of `Dockerfile.release`):

```bash
docker build -f Dockerfile.release --build-arg KEEP_VERSION=keep@0.4.13 -t keep:local .
```

Run it in memory. The image has no `/data` directory that the non-root user
can write to, so this example uses `--disable-persistence`. Keep then loses its
data when the container stops.

```bash
docker run --rm -p 7117:7117 -e KEEP_HOST=0.0.0.0 keep:local --disable-persistence
```

For durable storage, mount a writable volume at the data directory and make
sure it is owned by uid `65532`, as in `k8s/base/statefulset.yaml`
(`fsGroup: 65532`, data mounted at `/data`, `KEEP_DATA_DIR`).

Docker commands in this page are not tested.

## Set and get a key

With the server from the previous section running on `127.0.0.1:7117`, write a
value. The body is a JSON object with a `value` field:

```bash
curl -X PUT http://127.0.0.1:7117/kv/hello -H 'content-type: application/json' -d '{"value":"world"}'
```

Read it back. The response is `{"key":"hello","value":"world"}`:

```bash
curl http://127.0.0.1:7117/kv/hello
```

## With an API token

Two optional tokens protect the data routes:

- `KEEP_API_TOKEN` (or `--api-token`): read and write.
- `KEEP_API_TOKEN_READONLY` (or `--api-token-readonly`): `GET` and `HEAD` only.

Start the server with both tokens:

```bash
keep --api-token <read-write-token> --api-token-readonly <read-only-token>
```

Send the token as a bearer header:

```bash
curl -H 'Authorization: Bearer <read-write-token>' http://127.0.0.1:7117/kv/hello
```

With a token set, a missing or wrong bearer token returns `401` with error
`unauthorized`. A read-only token that tries to write returns `403` with error
`forbidden`.

## Check health

```bash
curl http://127.0.0.1:7117/healthz
curl http://127.0.0.1:7117/readyz
curl http://127.0.0.1:7117/info
```

`/healthz` returns `ok` while the process is alive. `/readyz` returns `ok` while
the server accepts traffic. `/info` returns a JSON summary of the server state,
including the key count and the version.
