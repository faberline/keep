# Version compatibility policy

keep is released as `keep@X.Y.Z` (a milestone `keep@X.Y.Z` in the issue tracker). This page is the rule for what must keep working between release N and release N+1 (the next release).

## On-disk format

- snapshot file version: 2
- snapshot file magic: `KVSN0001`
- backup payload version: 1
- WAL file: written by faberline/core `storage-durable` (framed log). The retired `KVWAL001` WAL format is refused at startup and the file is not changed.

Rules:
1. Release N+1 reads every file that release N wrote. A snapshot or backup from N restores on N+1.
2. A version that a release does not know is refused at startup with an error. keep never guesses and never deletes or truncates the file.
3. A change of any version number above ships only in a minor or major release, with a migration note in the release notes. A patch release never changes a file format.
4. Downgrade (N+1 data read by N) is not supported. Restore a backup taken before the upgrade instead.

## Wire compatibility

- HTTP API: described by `/openapi.json`. Release N+1 only adds fields, endpoints and optional parameters. A removal or a changed meaning needs a major release.
- Raft peer protocol: provided by faberline/core `raft-runtime` at the pinned tag (see the `Cargo.toml` of the release). Release N and N+1 must use the same peer protocol, so replicas of both releases can run in one group during an upgrade.

## Upgrade order

Upgrade one replica at a time. Wait until the replica is ready (`/readyz` answers 200) and has caught up before you restart the next one. With 3 replicas the group keeps quorum and keeps serving reads and writes during the whole upgrade.

## Announcing a format change

A format or wire change is announced in three places before it ships: the release notes of N (a warning), the issue that makes the change (label `type:breaking`, a section "Compatibility"), and this page (the numbers above change in the same pull request that changes the format).
