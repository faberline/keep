# keep operations

Start with these pages:

- [runbook](../product/runbook.md): health, metrics, node down, disk full, restore and token rotation.
- [quickstart](../product/quickstart.md): how to run keep for the first time.

## Commands

- `keep backup`: writes a consistent snapshot of keep's on-disk state to a backup destination.
- `keep restore`: loads the newest backup artifact into a data directory, offline, with no server.
- `keep spec`: prints keep's integration spec offline, by default the OpenAPI 3 JSON document; `keep spec gen --lang ts|py|rust --out <dir>` generates a typed client instead.
