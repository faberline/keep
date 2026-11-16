# keep documentation

keep is a cloud-native, multi-core key-value and claim-check store. It is the
data plane for the loom and relay workers, and a Redis and Dragonfly
replacement. It serves HTTP/2 with an OpenAPI document. The engine is sharded
and persists to disk in RAM and on disk tiers.

| Page | What it holds |
|------|---------------|
| [architecture.md](architecture.md) | The crates, the layers and where state lives. |
| [glossary.md](glossary.md) | The terms used in keep, each in one sentence. |
| [domain/keyspace.md](domain/keyspace.md) | The keyspace: value types, the key rule and the operation groups. |
| [adr/0001-http2-openapi-transport.md](adr/0001-http2-openapi-transport.md) | Why the transport is HTTP/2 with OpenAPI and no raw TCP. |
| [operations/README.md](operations/README.md) | The operator index: the runbook and the backup and restore commands. |
| [product/quickstart.md](product/quickstart.md) | How to run keep for the first time. |
| [product/runbook.md](product/runbook.md) | Health, metrics, node down, disk full, restore and token rotation. |
| [product/compatibility.md](product/compatibility.md) | The version compatibility policy. |
| [product/performance.md](product/performance.md) | The performance baseline and its gates. |
