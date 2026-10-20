# Fee accounting

The settlement core uses `FEE_RATE_SCALE` fixed-point rates. Taker fees are
rounded up using the same checked calculation for both bid reservation and
settlement, so a fill cannot create an under-collateralized taker.

Protocol fees are liabilities keyed by token in `FeeAccumulator`. They are
not part of `TraderAccount.available_balance`, `locked_balance`, or
`total_balance`; withdrawal code therefore cannot spend them. A fee-bearing
settlement records the fee liability alongside the trader quote transfers,
and conservation includes both owners.

Version one resolves either the market-wide rate or one bounded, already
authorized `FeeTier` in O(1). No historical-volume scan is performed. The
resolved rate is applied while the fill is settled, so later tier changes do
not alter completed fills.
