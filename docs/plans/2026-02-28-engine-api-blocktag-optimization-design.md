# Engine API Block Tag Support & FCU Optimization Design

**Date:** 2026-02-28
**Branch:** refactor/engine-api
**Status:** Approved

## Context

### Performance Baseline (sync-test.sh, 120s each, from block 0)

| Metric | Geth | Reth |
|--------|------|------|
| Avg BPS | 86.87 | 39.92 |
| Peak BPS | 92.70 | 48.40 |
| Est. full sync | 2d 19h | 6d 2h |

### Bottleneck Analysis

Engine timing log analysis (67K blocks) reveals:

| Component | Avg Time | % of Cycle |
|-----------|----------|-----------|
| Reth engine processing (new_payload + FCU) | 0.35ms | 1.5% |
| External overhead (morphnode + HTTP/RPC) | ~24ms | 98.5% |

Reth internal processing is already fast. The ~13ms gap vs geth (~11ms external) comes from:
1. HTTP/RPC layer overhead (jsonrpsee/hyper/tokio vs Go native HTTP)
2. Two sequential engine tree round-trips per block (new_payload + FCU)
3. Per-FCU `save_finalized_block_number` / `save_safe_block_number` DB writes

### Geth PR #277 Reference

[morph-l2/go-ethereum#277](https://github.com/morph-l2/go-ethereum/pull/277) adds:
- `engine_setBlockTags(safeHash, finalizedHash)` — dedicated RPC for block tag updates
- `NewL2Block` unchanged — does NOT accept block tags
- safe/finalized decoupled from block import
- finalized persisted to DB, safe is memory-only (initialized to finalized on restart)

## Design

### 1. New RPC: `engine_setBlockTags`

**Signature:**
```rust
// crates/engine-api/src/api.rs — MorphL2EngineApi trait
async fn set_block_tags(
    &self,
    safe_block_hash: B256,
    finalized_block_hash: B256,
) -> Result<(), MorphEngineApiError>;

// crates/engine-api/src/rpc.rs — MorphL2EngineApiServer RPC
#[method(name = "setBlockTags")]
async fn set_block_tags(
    &self,
    safe_block_hash: B256,
    finalized_block_hash: B256,
) -> RpcResult<()>;
```

**Implementation (builder.rs):**
```
set_block_tags(safe_hash, finalized_hash):
  1. Read current canonical head from EngineStateTracker
  2. Send FCU with:
     - head_block_hash = current canonical head (no change)
     - safe_block_hash = safe_hash
     - finalized_block_hash = finalized_hash
  3. This hits FCU "fast path" (head already canonical):
     → Only updates safe/finalized tags + DB persistence
     → No re-canonicalization
  4. Update EngineStateTracker forkchoice state
```

**Zero hash handling:** If either hash is `B256::ZERO`, skip that tag update (matching geth behavior).

### 2. Simplified Per-Block FCU

**Current flow (per NewL2Block):**
```
import_l2_block_via_engine():
  new_payload(block)
  fork_choice_updated(head=block, safe=current_safe, finalized=current_finalized)
  → Triggers save_finalized_block_number + save_safe_block_number DB writes
```

**Optimized flow:**
```
import_l2_block_via_engine():
  new_payload(block)
  fork_choice_updated(head=block, safe=ZERO, finalized=ZERO)
  → safe/finalized = ZERO → ensure_consistent_forkchoice_state skips DB writes
  → Only canonical head advancement happens
```

**Change in builder.rs `import_l2_block_via_engine`:**
- Remove `current_forkchoice_state()` lookup for safe/finalized
- Always pass `B256::ZERO` for safe_block_hash and finalized_block_hash
- Remove `mark_safe` parameter from function signature

### 3. Updated `new_safe_l2_block` Flow

**Current:**
```
new_safe_l2_block(data):
  import_l2_block_via_engine(data, None, mark_safe=true)
  → FCU sets safe_block_hash = data.hash
```

**Optimized:**
```
new_safe_l2_block(data):
  import_l2_block_via_engine(data, None)          // standard import, FCU with ZERO tags
  set_block_tags(safe=data.hash, finalized=ZERO)  // separate tag update
```

This decouples block import from tag management, matching geth's architecture.

### 4. EngineStateTracker Updates

The `EngineStateTracker` already tracks forkchoice state. After `set_block_tags`, update the tracked safe/finalized:

```rust
// In set_block_tags implementation:
self.engine_state_tracker.record_local_forkchoice(ForkchoiceState {
    head_block_hash: current_head,
    safe_block_hash: safe_hash,
    finalized_block_hash: finalized_hash,
});
```

## File Changes

| File | Change |
|------|--------|
| `crates/engine-api/src/api.rs` | Add `set_block_tags` to `MorphL2EngineApi` trait |
| `crates/engine-api/src/rpc.rs` | Add `set_block_tags` to `MorphL2EngineApiServer`, implement handler |
| `crates/engine-api/src/builder.rs` | Implement `set_block_tags`, simplify FCU in `import_l2_block_via_engine`, update `new_safe_l2_block` |

## Expected Impact

- **Performance:** ~5-15% improvement from eliminating per-block safe/finalized DB writes
- **Correctness:** Block tags (safe, finalized) properly supported — currently finalized is never updated
- **API parity:** Aligned with geth PR #277 `engine_setBlockTags`

## Future Work (Not in Scope)

1. **RPC/HTTP layer optimization** — Profile jsonrpsee/hyper overhead to close the ~13ms gap vs geth
2. **Combine new_payload + FCU** — Merge into single engine tree message (requires reth fork changes)
3. **Skip deferred trie task** — When state root is skipped, the background rayon trie computation is wasted CPU (but non-blocking, low impact)
4. **Batch block import** — Accept multiple blocks per RPC call to amortize per-request overhead
