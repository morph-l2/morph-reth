# Onyx Millisecond Timestamps

Onyx extends Morph block time with a `timestampMillisPart` remainder in the
range `0..1000`. The canonical full timestamp is:

```text
timestampMillis = timestamp * 1000 + timestampMillisPart
```

The block hash commits to this field by appending it as an optional trailing
header field in the same flat RLP list as the Ethereum header fields. This
matches geth's header extension pattern and avoids a nested `[header, millis]`
encoding.

## Cross-Client DA Requirement

If `timestampMillisPart` is non-zero after Onyx, it is part of block identity.
Any L1 DA or batch commitment format used to re-derive L2 blocks must therefore
carry the millisecond remainder alongside the seconds timestamp.

If the DA layer only commits seconds, a block re-derived from L1 data loses the
millisecond remainder and recomputes a different block hash. That would break
safe import, reorg repair, and cross-client consistency for both morph-reth and
Morph geth.

The Onyx specification should define the DA/batch encoding change together with
the execution-client header and Engine API changes.
