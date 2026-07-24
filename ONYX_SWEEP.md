# Onyx Recoverable Deposit Sweeping

## Purpose and scope

Onyx adds consensus-level, best-effort sweeping of supported ERC-20 balances
from exchange deposit EOAs to a registered master address. The deposit does not
need native currency: a master or its operator can sponsor onboarding, and a
successful token inflow can trigger settlement in the same transaction.

The protocol has three fixed components:

| Component | Responsibility |
| --- | --- |
| `SweepRegistry` | Stores deposit ownership, master approval, token policy, pause state, and the atomic resolver used by the execution layer (EL). |
| `SweepDeposit` | Minimal EIP-7702 delegate that can only transfer the deposit's full supported-token balance to the resolved master. |
| Onyx EL hook | Discovers triggers after successful transactions, validates all policy and code pins, executes bounded sweeps, and appends settlement logs. |

This is not a general account-abstraction executor. It cannot execute arbitrary
calls, sweep native currency, choose a recipient at execution time, or persist a
retry queue. The delegate keeps payable receive/fallback paths so the address
retains ordinary EOA receive behavior; any native balance remains at the deposit
and must be handled by a separately signed native transfer.

## Consensus configuration

Onyx chains must set all three non-zero values under `config.morph`:

```json
{
  "sweepRegistryAddress": "0x...",
  "sweepDepositDelegateAddress": "0x...",
  "sweepDepositDelegateCodeHash": "0x..."
}
```

The Registry address, delegate address, and delegate runtime code hash are
consensus parameters. Every execution client must use the same values. Onyx
block-environment construction fails if any value is absent or zero.

The Registry is a direct, non-proxy deployment. `SweepDeposit` has no upgrade
path and fixes its own deployment address as the immutable synthetic executor. The Registry
also stores the delegate address and runtime hash as immutables and derives the
only accepted deposit designator hash:

```text
keccak256(0xef0100 || sweepDepositDelegateAddress)
```

Deploy and verify both contracts before scheduling Onyx. Record the deployed
delegate runtime hash, compare it with both the Registry immutable and chain
configuration, and verify the Registry is the audited direct runtime rather
than a proxy. The deployment script in the `morph` repository performs these
address and hash checks.

Changing any of the three chain parameters requires a coordinated network
upgrade. For routine incidents, use the Registry pause switch instead.

## Deposit onboarding

Onboarding uses two independent signatures from the deposit EOA:

1. An EIP-7702 authorization designates the configured `SweepDeposit`
   implementation.
2. An EIP-712 `SweepAuthorization` authorizes one Registry record mapping the
   deposit to a master.

The EIP-712 domain is:

```text
name              = "SweepRegistry"
version           = "2"
chainId           = current chain ID
verifyingContract = SweepRegistry address
```

The signed type is:

```text
SweepAuthorization(
    address deposit,
    address master,
    address registry,
    uint256 chainId,
    uint256 nonce,
    uint64 deadline,
    address delegate,
    bytes32 delegateCodeHash,
    bytes32 mode,
    bytes32 sweepScope
)
```

with:

```text
registry   = SweepRegistry address
delegate   = configured SweepDeposit address
mode       = keccak256("MORPH_SWEEP_V2")
sweepScope = keccak256("PINNED_ERC20_BALANCE_TO_MASTER_ONLY")
```

Binding the authorization to the chain, Registry, master, delegate code, mode,
scope, nonce, and deadline prevents cross-chain, cross-deployment, recipient,
implementation, and old-mode replay. The Registry nonce is separate from the
EOA nonce used by EIP-7702. A successful registration increments the Registry
nonce; re-registration after disable must use the new value.

The Registry owner must first approve the master. The master may register
directly or authorize per-master operators with `setSweepOperator`. A master or
operator normally submits an EIP-7702 type-4 transaction that includes the
deposit's delegation authorization and calls `registerSweep`; delegation is
installed before the Registry call, so the deposit needs no native balance.
`registerSweeps` supports atomic batching.

Registration rejects:

- zero, self-sweep, and reserved Morph system-segment addresses;
- an unapproved master or unauthorized submitter;
- a master that is itself an active deposit, or a deposit that is an active
  master;
- an already-active deposit, stale Registry nonce, expired or invalid
  signature;
- a deposit designator or delegate runtime that does not match the fixed pins.

The deposit, master, or master operator may call `disableSweep`, including while
the system is paused. Removing a master from the approval list immediately
makes all of its records resolve to zero policy as a fail-closed bulk stop;
disable individual records as part of permanent offboarding.

## Registry policy

The Registry owner controls:

- approved master addresses;
- the global fail-closed pause;
- a per-token tuple of `enabled`, exact runtime `codeHash`, and non-zero
  `minimumAmount`.

`resolveSweep(token, deposit)` is the frozen consensus-facing selector. It
returns exactly the atomic 96-byte tuple:

```text
(master, tokenCodeHash, minimumAmount)
```

It returns zero policy when paused, the deposit is inactive, the token policy
is disabled, the live token runtime hash changed, the live delegate hash
changed, or the deposit no longer has the exact EIP-7702 designator. The EL
rejects malformed, zero, or otherwise inconsistent resolver output and
independently rechecks the token, deposit, and delegate code pins.

`pokeSweep(token, deposit)` is permissionless, but succeeds only when the
current resolver policy is active. It emits `SweepRequested` so an existing
balance can be retried without another token transfer.

## Execution semantics

The hook runs only from Onyx onward, only with explicit canonical execution
authority, and only after the main transaction succeeds. Reverted or halted
transactions do not trigger sweeping. Ordinary `eth_call`, gas estimation, and
other non-canonical executions do not run the hook.

Candidate triggers are:

- an exact three-topic, 32-byte-data ERC-20
  `Transfer(address,address,uint256)` log, using the log emitter as `token` and
  the low 20 bytes of the `to` topic as `deposit`, from main execution;
- a canonical `SweepRequested(token,deposit)` emitted by the configured
  Registry during main execution; or
- a successful, non-zero, non-self fee-token reimbursement credit, whether the
  transfer used an EVM call or a direct storage-slot update, when the caller's
  current EIP-7702 designator exactly matches the block-configured delegate.
  This is one exact trigger-only candidate and does not fabricate a receipt log.

For a successful transaction, discovery covers main execution logs, followed by
the exact fee-token reimbursement recipient. Fee-token deduction credits the
chain fee recipient rather than an exchange deposit and is deliberately excluded
from discovery. Both deduction and reimbursement logs remain in the receipt and
count toward receipt-relative sweep log offsets, but arbitrary logs emitted by
fee accounting do not create additional candidates. This avoids charging sweep
preflight budget to internal log noise. Refund candidates are recorded only
when the caller is already delegated to the chain-configured sweep
implementation, so ordinary fee-token traffic cannot starve the block budget.
A reverted or halted main transaction does not run the hook, even when Morph
fee-accounting logs survive into its receipt.

Registry requests are processed before transfer triggers. A `(token, deposit)`
pair is deduplicated within a transaction. Registry requests are additionally
deduplicated across the block, while a later real transfer remains eligible;
this avoids repeated pokes suppressing a new inflow.

For each trigger, the EL:

1. Statically calls `resolveSweep(token, deposit)`.
2. Rechecks the ordinary token runtime hash, exact deposit designator, and fixed
   delegate runtime hash.
3. Reads deposit and master balances with bounded static `balanceOf` calls and
   skips zero or below-minimum balances.
4. Starts an isolated journal checkpoint and calls the deposit with
   `SweepDeposit.sweep(token, master, fullDepositBalance)`.
5. Verifies the deposit ended at zero, the master's balance increased by
   exactly that amount, and exactly one canonical matching token `Transfer`
   log was emitted.
6. Commits that candidate or reverts its checkpoint without reverting the main
   transaction.

The synthetic top-level caller and `tx.origin` are the fixed delegate address.
The EIP-7702 code executes in the deposit's account context, so the token still
observes `msg.sender == deposit`. No private key controls the synthetic caller,
and the deployed delegate rejects ordinary external callers.

On success, the receipt retains the main transaction logs, then appends the
token's canonical settlement `Transfer`, followed by an EL-synthesized
Registry-addressed:

```text
Swept(token, deposit, master, amount, transferLogOffset)
```

`transferLogOffset` is the absolute index of the matching settlement
`Transfer` in that receipt. Business failures produce no settlement logs; they
are classified for metrics and processing continues with the next candidate,
except when the remaining resource budget cannot fit more work.

## Strict side-effect boundary

A sweep candidate commits only when its persistent journal effects stay within
all of these bounds:

- storage writes belong only to the candidate token;
- account touches belong only to the token or deposit;
- no native-currency balance change or transfer;
- no nonce mutation;
- no account creation, code change, or destruction;
- no transient-storage mutation;
- exactly one log, the canonical
  `Transfer(deposit, master, fullDepositBalance)`.

Account and storage warming are non-persistent and allowed. A violation reverts
the entire candidate checkpoint. These checks contain a supported token's
effects to its own state, but do not prove that the token's accounting is
economically honest.

## Resource limits and denial-of-service behavior

Sweeping uses a separate deterministic system budget; it does not increase the
sender's charged transaction gas or cumulative receipt gas.

| Resource | Per transaction | Per block |
| --- | ---: | ---: |
| Raw trigger preflights | 64 | 149 |
| Policy-eligible executions | 16 | 44 |
| Fixed system budget | up to 10,400,000 in canonical execution | 22,400,000 |

Each raw trigger preflight debits the full gas forwarded to its resolver and
two balance calls: `50,000 + 2 * 50,000 = 150,000` units. A policy-eligible
execution debits the full gas forwarded to its delegate and two post-execution
balance calls: `250,000 + 2 * 50,000 = 350,000` more units, for 500,000 units
when a candidate reaches execution. These conservative fixed debits bound the
worst case even when every internal call exhausts its allowance. The fixed
system allowance may stop processing before a numeric trigger cap is reached.

Untrusted contracts can emit Transfer-shaped logs. Such logs can consume the
bounded preflight allowance, but cannot consume an execution slot unless the
Registry policy and every code/delegation/balance check pass. This bounds block
cost but does not guarantee that every eligible balance is swept in its trigger
transaction.

There is deliberately no consensus retry queue. A candidate skipped because of
budget, minimum amount, pause, code mismatch, or token behavior remains in the
deposit until another qualifying transfer or a later permissionless
`pokeSweep`. Exchanges should monitor deposit balances and issue pokes in later
blocks. Never treat the presence of the inflow transaction alone as proof that
settlement completed; require the matching `Swept` event and verify balances.

## Trace replay

Historical execution must reproduce both sweep state changes and block-global
budget/deduplication state. Canonical trace replay therefore:

1. establishes an isolated replay scope for the block;
2. replays transaction hashes in canonical order;
3. supplies each transaction with a plan derived from the committed effects of
   earlier transactions; and
4. clears replay state at the requested target, end of block, mismatch, error,
   or scope exit.

Only committed transaction effects advance the block session. Discarded
payload-builder attempts do not consume budget. This keeps state-at-transaction,
receipts, and trace replay aligned with canonical block execution while leaving
non-canonical calls side-effect free.

## Security and compatibility assumptions

- The Registry owner is trusted to approve the correct masters and exact token
  runtime hashes, choose safe minimums, pause promptly, and protect its key.
- Deposit private keys remain security-sensitive. EIP-7702 delegation does not
  revoke ordinary EOA signing authority; loss of a deposit key still requires
  immediate Registry disable and custody response.
- Native currency and unknown calldata are accepted with EOA-like no-op
  behavior, but native balances are never automatically swept or represented by
  `Swept`; monitor and collect them through the exchange's normal signed native
  transfer process.
- The master address is the final recipient. Compromise or misconfiguration of
  a master is not recoverable by the sweep protocol.
- Supported tokens must return a canonical 32-byte `balanceOf`, implement
  standard or no-return `transfer`, move the full amount, and emit exactly one
  canonical Transfer log. Fee-on-transfer, rebasing, callback-heavy, multi-log,
  or otherwise non-standard tokens will normally fail closed.
- A runtime code-hash pin is not an implementation pin for transparent, UUPS,
  beacon, diamond, or other upgradeable proxies: their proxy bytecode can stay
  unchanged while behavior changes. The current policy should not enable proxy
  tokens unless a future policy also pins and validates the implementation and
  upgrade authority. The same warning applies to apparently direct runtimes
  that use `DELEGATECALL`, `CALLCODE`, or another external execution indirection:
  the pinned token bytes alone do not pin the code that implements its
  accounting. Strict journal checks limit cross-contract damage but cannot make
  mutable token accounting trustworthy.
- A direct token with the pinned runtime can still implement adversarial
  accounting inside its own storage. Only audited, operationally approved
  tokens should be enabled.
- The Registry and delegate themselves must remain direct, immutable
  deployments at their configured addresses. Never substitute proxy variants.

## Operational checklist

Before activation:

1. Deploy the no-argument `SweepDeposit` and verify its executor equals its address.
2. Verify its runtime hash independently.
3. Deploy the direct `SweepRegistry(owner, delegate, delegateCodeHash)`.
4. Put the exact Registry/delegate/hash triplet in every client's genesis or
   chain configuration.
5. Approve masters and enable only audited direct-token runtimes with deliberate
   minimum amounts.
6. Exercise EIP-7702 onboarding, inflow settlement, receipt logs, pause,
   disable, and poke on the target chain.
7. Activate Onyx only after all clients and state agree.

Continuously alert on `morph.sweep.failures_total`,
`morph.sweep.failures_by_reason`, `morph.sweep.code_skipped_total`, and system
budget saturation. During an incident, pause globally first, leave revocation
available, reconcile unswept balances, fix policy or delegation, unpause, and
poke affected pairs.
