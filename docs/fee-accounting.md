# Fee accounting

The settlement core uses `FEE_RATE_SCALE` fixed-point rates. Each market has two
rates, both in `[0, MAX_FEE_RATE]`:

- `taker_fee_rate`, charged to the taker on each fill;
- `maker_fee_rate`, charged to the maker on each fill.

Rates are non-negative. Maker rebates are out of scope for v1, and a rate of
zero charges nothing.

## Formulas

Every fee is rounded up, so a non-zero obligation is never under-collected.
With `Up(x)` the ceiling of `x` (protocol spec section 4.2):

```text
fee(q, r)          = Up(q * r / FEE_RATE_SCALE)
maker_fee(fill)    = fee(fill_quote, maker_fee_rate)
taker_fee(fill)    = fee(T + fill_quote, taker_rate) - fee(T, taker_rate)
```

`T` is the quote the same taker already filled earlier in the transaction, and
is zero for its first fill. The taker fee is therefore charged on the running
total. The fees of consecutive fills sum to the fee of their total, so splitting
a fill across several makers never changes what the taker pays. The maker fee
has no running total: a maker is filled at most once per transaction.

## Settlement legs

A fill moves fees out of the trader quote legs and into protocol liabilities:

| Side  | Quote leg, seller is maker    | Quote leg, buyer is maker     |
|-------|-------------------------------|-------------------------------|
| Maker | `+quote - maker_fee`          | `-(quote + maker_fee)`        |
| Taker | `-(quote + taker_fee)`        | `+quote - taker_fee`          |

Each fee is recorded as a `fee_liabilities` entry, so the quote legs and the
liabilities sum to zero. A zero fee records no entry, so a zero-fee market has
no hidden deductions.

Protocol fees are liabilities keyed by token in `FeeAccumulator`. They are not
part of `TraderAccount.available_balance`, `locked_balance`, or `total_balance`,
so withdrawal code cannot spend them. Conservation includes both owners.

## Bid reservation

A resting bid can later fill as a maker, so its lock cannot assume the taker
role. A bid for quote `q` locks:

```text
required_bid_reservation(q) = q + max(taker_fee(q), maker_fee(q))
```

## Tiers

Version one resolves either the market-wide rate or one bounded, already
authorized `FeeTier` in O(1). A tier replaces only the taker rate, and it is
validated against `MAX_FEE_RATE`. No historical-volume scan is performed. The
resolved rate is applied while the fill is settled, so later tier changes do
not alter completed fills.

## Test vectors

These vectors are checked in `libs/clob_settlement_core/src/lib.rs`. Any SDK
preview must produce the same numbers.

| Quote   | Taker rate | Maker rate | Taker fee | Maker fee |
|---------|------------|------------|-----------|-----------|
| 1000000 | 100000     | 50000      | 1000      | 500       |
| 1000    | 100000     | 200000     | 1         | 2         |

Split fills with `taker_rate = 1`: three fills of one quote atom each pay a
total taker fee of `1`, the same as one fill of three atoms.
