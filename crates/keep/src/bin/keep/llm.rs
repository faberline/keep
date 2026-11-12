//! `keep llm` — agent-facing topics, offline.

use anyhow::Result;

use crate::common::TOOL;

/// `keep llm` flags.
#[derive(clap::Args, Debug)]
pub(crate) struct LlmArgs {
    /// Topic id (`outline` lists them all).
    #[arg(default_value = "outline")]
    topic: String,
    /// Output format: `md` (default) or `json`.
    #[arg(long, default_value = "md")]
    format: String,
}

/// keep's agent-facing `llm` topics — the single in-code source of truth.
const TOPICS: &[cli_std::llm::Topic] = &[
    cli_std::llm::Topic {
        id: "http",
        summary: "the HTTP/2 + OpenAPI surface (KV, batch, scan, locks, collections, probes)",
        body: "# keep — HTTP/2 API surface\n\n\
            One port speaks HTTP/1.1 and HTTP/2 cleartext (h2c, prior-knowledge). JSON \
            values, or raw `application/octet-stream` blobs on the value path.\n\n\
            - `GET|PUT|DELETE|HEAD /kv/{key}` — scalar value (`?ttl_ms=` on PUT).\n\
            - `POST /kv/{key}/incr|cas|setnx` — atomic integer / compare-and-swap.\n\
            - `POST /kv:mset|kv:mget|kv:mdel` — batch.\n\
            - `GET /kv?prefix=&limit=` — prefix scan.\n\
            - `POST|DELETE /locks/{name}` — owner+TTL advisory locks.\n\
            - `/hashes /sets /zsets /lists` — collections.\n\
            - `/healthz /readyz /metrics /openapi.json /docs` — probes, metrics, OpenAPI.\n\n\
            The full document: `GET /openapi.json` (served by the binary).\n",
    },
    cli_std::llm::Topic {
        id: "claim-check",
        summary: "the relay/loom worker data plane — inputs/results by id, namespaces, tokens",
        body: "# keep — claim-check data plane\n\n\
            keep is loom/relay's result store: a worker GETs its input and PUTs its result \
            by message id; the producer mirrors it.\n\n\
            - `GET|PUT /inputs/{id}` — job input payload.\n\
            - `GET|PUT /results/{id}` — job result payload.\n\n\
            Bytes-first (octet-stream), durable before the write is acked.\n\n\
            **Scoped tokens** (#445/#446): set `KEEP_TOKEN_SECRET` to require an HMAC \
            `Authorization: Bearer <token>` scoped to the bare input/result key on worker ops.\n\n\
            **Namespaces** (#464): send `X-Keep-Namespace` (loom sends `LOOM_NAMESPACE`) and \
            keep stores at `{ns}::{kind}:{id}` — applied after the token check, so token scope \
            stays the bare key. Absent header ⇒ bare key (single-tenant back-compat).\n",
    },
    cli_std::llm::Topic {
        id: "operate",
        summary: "run / configure / deploy — flags, env vars, persistence, k8s probes",
        body: "# keep — operating the server\n\n\
            Bare `keep` runs the server (env-driven; flags override). Key knobs:\n\n\
            - `--host/--port` (`KEEP_HOST` / `KEEP_PORT`, default `127.0.0.1:7117`).\n\
            - `--shards` (`KEEP_SHARDS`) — engine shard count for multi-core scaling.\n\
            - `--data-dir` (`KEEP_DATA_DIR`) — WAL + snapshot disk tier; cold start recovers it.\n\
            - `--disable-persistence` — in-memory only.\n\
            - `KEEP_API_TOKEN` (read-write) and `KEEP_API_TOKEN_READONLY` (GET/HEAD only) — Bearer tokens for the data plane.\n\
            - `--tls-cert/--tls-key` (`KEEP_TLS_CERT` / `KEEP_TLS_KEY`) — serve the public API over HTTPS.\n\
            - `--node-id/--node-count/--shard-count/--peers` — cluster topology.\n\
            - `KEEP_TOKEN_SECRET` — enable claim-check token enforcement.\n\n\
            Cloud-native: `/healthz` + `/readyz` probes and a SIGTERM-aware graceful drain \
            (`--grace-secs`) so k8s rolls pods without dropping requests.\n",
    },
];

/// `keep llm` — print the agent-facing topics (offline; no server).
pub(crate) fn run(args: LlmArgs) -> Result<()> {
    // Offline: no engine, no server, no I/O beyond stdout.
    let out = cli_std::llm::render(
        TOOL.project,
        TOOL.version,
        TOPICS,
        &args.topic,
        cli_std::llm::Format::parse(&args.format),
    )?;
    println!("{out}");
    Ok(())
}
