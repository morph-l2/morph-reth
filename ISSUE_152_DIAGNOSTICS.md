# Issue #152 persistence diagnostics

This branch is a temporary diagnostic build for
[issue #152](https://github.com/morph-l2/morph-reth/issues/152). It keeps the
production `v1.1.0` Reth base (`v2.4.0`) and pins a diagnostic Reth revision
that adds logging and invariant checks only. It does not intentionally change
Engine Tree or persistence behavior.

The added tracing targets record:

- every block accepted by `newPayload` and canonicalized by
  `forkchoiceUpdated` in the Morph import path;
- the full ordered block list selected for each persistence batch;
- the same list when `save_blocks` starts and when its commit returns;
- the old and new durable frontiers when the Engine Tree receives the result;
- the frontier used when persisted blocks are removed from memory; and
- errors if a selected batch skips the durable frontier, is not a contiguous
  number/hash chain, or fails to advance the frontier.

No diagnostic database reads are added. This is intentional: on the original
ZFS configuration, extra reads update `atime` and would alter the storage
workload being investigated.

## Build

```bash
git clone --branch codex/issue-152-persistence-logs \
  https://github.com/morph-l2/morph-reth.git morph-reth-issue-152
cd morph-reth-issue-152
docker build --platform linux/amd64 \
  --build-arg BUILD_PROFILE=profiling \
  --build-arg MORPH_GIT_SHA="$(git rev-parse HEAD)" \
  -t morph-reth:issue-152 .
docker run --rm morph-reth:issue-152 --version
```

## Run

Use a disposable, freshly restored copy of the paired snapshot and the same
untuned ZFS layout and node arguments that reproduced the issue. Replace only
the morph-reth image. Add these global logging arguments to the existing
command:

```text
--log.file.directory /data/issue-152-logs
--log.file.name reth-issue-152.log
--log.file.filter info,morph::engine::issue_152=debug,engine::tree::issue_152=debug,engine::persistence::issue_152=debug
--log.file.max-size 200
--log.file.max-files 10
```

For example, on the reporter's Linux host, replace the two `/absolute/path`
values:

```bash
mkdir -p "$PWD/issue-152-logs"
docker run --rm --name morph-reth-issue-152 \
  --network host \
  --user "$(id -u):$(id -g)" \
  -v /absolute/path/reth-data:/data \
  -v /absolute/path/jwt.hex:/jwt.hex:ro \
  -v "$PWD/issue-152-logs:/logs" \
  morph-reth:issue-152 node \
  --chain mainnet \
  --datadir /data \
  --authrpc.jwtsecret /jwt.hex \
  --log.file.directory /logs \
  --log.file.name reth-issue-152.log \
  --log.file.filter 'info,morph::engine::issue_152=debug,engine::tree::issue_152=debug,engine::persistence::issue_152=debug' \
  --log.file.max-size 200 \
  --log.file.max-files 10
```

Append any additional flags from the original reproducing morph-reth command.

Reth appends the chain name to the configured log directory, so the files in
this example will be below `issue-152-logs/mainnet/`. The narrow filter avoids
enabling all Reth debug logs and keeps the diagnostic workload small.

After the replay, stop morph-node, allow morph-reth to finish persistence, stop
morph-reth cleanly, restart it, and run the same missing-block scanner. Preserve:

1. all `reth-issue-152.log*` files;
2. the first missing block range, if any;
3. the first and last `ApplyBlockV2 success` lines from morph-node; and
4. the exact container command and ZFS dataset properties.

The diagnostic records can be extracted without unrelated logs with:

```bash
rg "issue-152" "$PWD/issue-152-logs" \
  -g 'reth-issue-152.log*' > issue-152-persistence.log
```

This build is for disposable reproduction data only and must not be used as a
production release.
