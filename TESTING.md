# Testing Morph Protocol Behavior

Tests in this repository protect protocol behavior, not implementation shape.
Prefer the fastest layer that crosses the interface where the behavior is
observable:

1. Unit tests for pure rules, encodings, and arithmetic.
2. Slice tests for interactions between real Morph modules with local adapters.
3. In-process node tests for Engine RPC, canonical chain, state, pool, and index behavior.
4. Spawned-binary smoke tests only for process, authentication, and persistence boundaries.

Cross-repository Morph consensus-node behavior is outside this repository's
test boundary. In-process Engine RPC tests use a test driver in place of the
consensus node and therefore do not validate Tendermint state, L1 synchronization
pointers, or derivation cursors.

## Behavior contracts

Every non-trivial regression test should state:

- the protocol behavior being protected;
- the fault or regression that would violate it;
- the observable outcomes used as independent evidence;
- any behavior explicitly outside the test's scope.

Expected behavior must come from a reviewed protocol rule, system-contract
transition, or independently derived invariant. Another client can reveal a
disagreement, but its current behavior is not authoritative by itself. If
implementations, contracts, and written rules disagree, document and resolve
the behavior before turning it into a permanent assertion.

During development, temporarily perturb the targeted behavior and confirm that
the new test fails for the intended reason. Do not commit the perturbation.

## First deep scenarios

### Invalid payload recovery

- Behavior: an execution result with a mismatched receipts root is rejected.
- Fault: a valid assembled block is resealed with a different receipts root.
- Evidence: Engine RPC rejects it; canonical head and account state do not
  change; the unmodified block can then be imported.
- Out of scope: historical hardfork combinations and withdraw-trie validation.

### Canonical reorganization consistency

- Behavior: sibling reorganization is atomic across canonical state, txpool,
  and the reference index, and connected nodes converge on the new branch.
- Fault: one branch contains a referenced Morph transaction that the replacement
  sibling does not contain.
- Evidence: both nodes expose the replacement hash and equivalent state; the
  removed transaction returns to the pool; the old reference disappears only
  after the new branch is indexed.
- Out of scope: Tendermint and L1 derivation state.

### Mixed block limits

- Behavior: L1 messages remain a sequential prefix while gas, data-availability,
  and transaction-count limits constrain pool inclusion without corrupting fee
  accounting.
- Fault: enough independent pool transactions are submitted to exceed one limit
  at a time.
- Evidence: block order and next L1 message index are valid; the selected limit
  is respected; included receipts and state agree; excluded transactions remain
  available to the pool.
- Out of scope: performance benchmarking and historical hardfork matrices.
