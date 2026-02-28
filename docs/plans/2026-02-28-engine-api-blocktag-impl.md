# Engine API Block Tag Support Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add `engine_setBlockTags` RPC and simplify per-block FCU calls to align with geth PR #277 and improve sync performance.

**Architecture:** Add a new `set_block_tags(safe, finalized)` method that leverages reth's FCU fast path (head already canonical → only update tags). Simplify `import_l2_block_via_engine` to pass `B256::ZERO` for safe/finalized in every-block FCU, eliminating per-block DB writes for those tags. Update `new_safe_l2_block` to decouple tag updates from block import.

**Tech Stack:** Rust, jsonrpsee, reth engine tree, alloy types

---

### Task 1: Add `set_block_tags` to the Engine API Trait

**Files:**
- Modify: `crates/engine-api/src/api.rs:26-97`

**Step 1: Add method to trait**

Add after the `new_safe_l2_block` method (line 96):

```rust
    /// Set the safe and finalized block tags.
    ///
    /// This method updates the safe and finalized block pointers without
    /// importing any new block. It aligns with go-ethereum's `engine_setBlockTags`.
    ///
    /// Either hash can be `B256::ZERO` to skip updating that tag.
    ///
    /// # Arguments
    ///
    /// * `safe_block_hash` - Hash of the block to mark as safe
    /// * `finalized_block_hash` - Hash of the block to mark as finalized
    async fn set_block_tags(
        &self,
        safe_block_hash: B256,
        finalized_block_hash: B256,
    ) -> EngineApiResult<()>;
```

**Step 2: Run compile check**

Run: `cargo check -p morph-engine-api 2>&1 | head -20`
Expected: Compile errors about missing `set_block_tags` implementation (trait not satisfied)

**Step 3: Commit**

```bash
git add crates/engine-api/src/api.rs
git commit -m "feat(engine-api): add set_block_tags to MorphL2EngineApi trait"
```

---

### Task 2: Add `set_block_tags` to the RPC Server Trait

**Files:**
- Modify: `crates/engine-api/src/rpc.rs:18-53` (MorphL2EngineRpc trait)
- Modify: `crates/engine-api/src/rpc.rs:78-143` (MorphL2EngineRpcHandler impl)

**Step 1: Add RPC method to trait**

Add after `new_safe_l2_block` in the `MorphL2EngineRpc` trait (after line 52):

```rust
    /// Set the safe and finalized block tags.
    ///
    /// # JSON-RPC Method
    ///
    /// `engine_setBlockTags`
    #[method(name = "setBlockTags")]
    async fn set_block_tags(
        &self,
        safe_block_hash: B256,
        finalized_block_hash: B256,
    ) -> RpcResult<()>;
```

**Step 2: Add handler implementation**

Add after `new_safe_l2_block` handler in the `MorphL2EngineRpcServer for MorphL2EngineRpcHandler` impl block (after line 142):

```rust
    async fn set_block_tags(
        &self,
        safe_block_hash: B256,
        finalized_block_hash: B256,
    ) -> RpcResult<()> {
        tracing::debug!(
            target: "morph::engine",
            %safe_block_hash,
            %finalized_block_hash,
            "RPC setBlockTags called"
        );

        self.inner
            .set_block_tags(safe_block_hash, finalized_block_hash)
            .await
            .map_err(|e| {
                tracing::error!(target: "morph::engine", error = %e, "failed to set block tags");
                e.into()
            })
    }
```

**Step 3: Run compile check**

Run: `cargo check -p morph-engine-api 2>&1 | head -20`
Expected: Compile errors about missing `set_block_tags` on `RealMorphL2EngineApi`

**Step 4: Commit**

```bash
git add crates/engine-api/src/rpc.rs
git commit -m "feat(engine-api): add setBlockTags RPC handler"
```

---

### Task 3: Implement `set_block_tags` in `RealMorphL2EngineApi`

**Files:**
- Modify: `crates/engine-api/src/builder.rs:168-401` (MorphL2EngineApi impl block)

**Step 1: Add implementation**

Add after the `new_safe_l2_block` method (after line 401, before the closing `}` of the impl block):

```rust
    async fn set_block_tags(
        &self,
        safe_block_hash: B256,
        finalized_block_hash: B256,
    ) -> EngineApiResult<()> {
        let current_head = self.current_head()?;

        let forkchoice = alloy_rpc_types_engine::ForkchoiceState {
            head_block_hash: current_head.hash,
            safe_block_hash,
            finalized_block_hash,
        };

        self.provider.on_forkchoice_update_received(&forkchoice);

        let fcu_result = self
            .engine_handle
            .fork_choice_updated(forkchoice, None, Self::engine_api_version())
            .await
            .map_err(|e| MorphEngineApiError::ExecutionFailed(e.to_string()))?;

        self.ensure_payload_status_acceptable(
            &fcu_result.payload_status,
            "setBlockTags forkchoiceUpdated",
        )?;

        self.engine_state_tracker
            .record_local_forkchoice(forkchoice);

        tracing::info!(
            target: "morph::engine",
            %safe_block_hash,
            %finalized_block_hash,
            fcu_status = ?fcu_result.payload_status.status,
            "block tags updated"
        );

        Ok(())
    }
```

**Step 2: Run compile check**

Run: `cargo check -p morph-engine-api`
Expected: SUCCESS

**Step 3: Run existing tests**

Run: `cargo test -p morph-engine-api`
Expected: All existing tests pass

**Step 4: Commit**

```bash
git add crates/engine-api/src/builder.rs
git commit -m "feat(engine-api): implement set_block_tags via FCU fast path"
```

---

### Task 4: Simplify Per-Block FCU in `import_l2_block_via_engine`

**Files:**
- Modify: `crates/engine-api/src/builder.rs:496-559` (import_l2_block_via_engine)

**Step 1: Remove `mark_safe` parameter and simplify FCU**

Replace the function signature and FCU section. Change line 496-558:

Old signature (line 496-501):
```rust
    async fn import_l2_block_via_engine(
        &self,
        data: ExecutableL2Data,
        batch_hash: Option<B256>,
        mark_safe: bool,
    ) -> EngineApiResult<MorphHeader>
```

New signature:
```rust
    async fn import_l2_block_via_engine(
        &self,
        data: ExecutableL2Data,
        batch_hash: Option<B256>,
    ) -> EngineApiResult<MorphHeader>
```

Replace the FCU section (lines 522-540). Old code:
```rust
        let mut forkchoice = self.current_forkchoice_state()?;
        forkchoice.head_block_hash = data.hash;
        if mark_safe {
            forkchoice.safe_block_hash = data.hash;
        }

        self.provider.on_forkchoice_update_received(&forkchoice);

        let fcu_started = Instant::now();
        let fcu_result = self
            .engine_handle
            .fork_choice_updated(forkchoice, None, Self::engine_api_version())
            .await
            .map_err(|e| MorphEngineApiError::ExecutionFailed(e.to_string()))?;
        let fcu_elapsed = fcu_started.elapsed();
        self.ensure_payload_status_acceptable(&fcu_result.payload_status, "forkchoiceUpdated")?;

        self.engine_state_tracker
            .record_local_forkchoice(forkchoice);
```

New code:
```rust
        // FCU only advances canonical head. Safe/finalized tags are managed
        // separately via set_block_tags, matching geth's engine_setBlockTags design.
        let forkchoice = alloy_rpc_types_engine::ForkchoiceState {
            head_block_hash: data.hash,
            safe_block_hash: B256::ZERO,
            finalized_block_hash: B256::ZERO,
        };

        self.provider.on_forkchoice_update_received(&forkchoice);

        let fcu_started = Instant::now();
        let fcu_result = self
            .engine_handle
            .fork_choice_updated(forkchoice, None, Self::engine_api_version())
            .await
            .map_err(|e| MorphEngineApiError::ExecutionFailed(e.to_string()))?;
        let fcu_elapsed = fcu_started.elapsed();
        self.ensure_payload_status_acceptable(&fcu_result.payload_status, "forkchoiceUpdated")?;

        self.engine_state_tracker
            .record_local_forkchoice(forkchoice);
```

Also remove `mark_safe` from the timing log (line 548): delete the `mark_safe,` line.

**Step 2: Update callers**

In `new_l2_block` (line 345-347), change:
```rust
        let imported_header = self
            .import_l2_block_via_engine(data, batch_hash, false)
            .await?;
```
to:
```rust
        let imported_header = self
            .import_l2_block_via_engine(data, batch_hash)
            .await?;
```

In `new_safe_l2_block` (line 390-392), change:
```rust
        let header = self
            .import_l2_block_via_engine(executable_data.clone(), data.batch_hash, true)
            .await?;
```
to:
```rust
        let header = self
            .import_l2_block_via_engine(executable_data.clone(), data.batch_hash)
            .await?;

        // Update safe block tag separately, matching geth's decoupled design.
        self.set_block_tags(executable_data.hash, B256::ZERO).await?;
```

**Step 3: Remove unused `current_forkchoice_state` method**

The `current_forkchoice_state` method (lines 682-708) is no longer called by `import_l2_block_via_engine`. Check if any other code calls it. If not, remove it.

**Step 4: Run compile check**

Run: `cargo check -p morph-engine-api`
Expected: SUCCESS (possibly a warning about unused `current_forkchoice_state` if not removed)

**Step 5: Run all tests**

Run: `cargo test -p morph-engine-api`
Expected: All tests pass

**Step 6: Commit**

```bash
git add crates/engine-api/src/builder.rs
git commit -m "refactor(engine-api): simplify per-block FCU and decouple safe tag updates

Per-block FCU now only advances canonical head (safe/finalized = ZERO).
Safe/finalized tags are updated via set_block_tags, aligning with geth's
engine_setBlockTags design and eliminating per-block DB writes for tags."
```

---

### Task 5: Full Integration Test

**Files:**
- Read: existing tests in `crates/engine-api/src/builder.rs:788-1026`

**Step 1: Run full workspace check**

Run: `cargo check --all`
Expected: SUCCESS

**Step 2: Run full test suite**

Run: `cargo test --all`
Expected: All tests pass

**Step 3: Run clippy**

Run: `cargo clippy --all --all-targets -- -D warnings`
Expected: No warnings

**Step 4: Run format check**

Run: `cargo fmt --all -- --check`
Expected: Clean

**Step 5: Run sync test to measure improvement**

Run: `SKIP_GETH=1 TEST_DURATION=120 ./local-test/sync-test.sh`
Expected: Reth BPS should be >= 40 (baseline was 39.92)

**Step 6: Commit any fixes**

If clippy or fmt needed fixes:
```bash
git add -A
git commit -m "fix: address clippy and formatting issues"
```
