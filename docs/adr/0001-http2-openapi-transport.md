# 0001: HTTP/2 + OpenAPI transport

## Status

Accepted

## Context

keep was promoted from `cclab-kv`. The sharded engine and the tiered RAM and
disk persistence are unchanged by this decision. Only the transport changes.

## Decision

The transport is HTTP/2 plus OpenAPI, with no raw TCP. Polyglot workers use the
generated OpenAPI document. `keep spec gen --lang ts|py|rust --out <dir>` makes
a typed client for each language.

## Consequences

- No language-specific client has to be shipped.
- The engine and the persistence layers stay transport-agnostic.
