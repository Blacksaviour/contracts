# SO4 CLOB Protocol Specification

This document defines the consensus-critical behavior of the SO4 central limit order book (CLOB).
Contract and SDK code must follow these definitions. Where code and this document disagree, this
document wins and the code is a bug. The specification is split into two parts: trading semantics
(sections 1 to 8) and invariants with bounded matching (sections 9 to 11).

Conventions use the **MUST**, **MUST NOT**, and **MAY** keywords as in RFC 2119.

---

## 1. Scope

The v1 exchange is a **fully collateralized spot central limit order book** on Soroban. Every order
is backed by tokens locked in the trader's account before it can rest or match. Matching follows
price-time priority, in the style of Stellar's offers and Phoenix's FIFO matching. Storage and
execution are adapted to Soroban resource limits: every transaction performs a bounded amount of
work (section 10).

## 2. Non-goals for v1

The following are explicitly out of scope. They MUST NOT be implemented as part of v1 and no
behavior in v1 may depend on them.

- Leverage, margin, and borrowing.
- Perpetual contracts and funding rates.
- Liquidations.
- Synthetic positions and oracle-priced settlement.
- Market orders. Every v1 order is a limit order.
- Stop, post-only, immediate-or-cancel, and fill-or-kill orders. v1 time-in-force is limited to
  GTC and GTD (section 5.3).
- In-place order amendment. A trader changes an order by cancelling it and placing a new one.
- Trading fees. Fees are not defined by this document. Any fee MUST be applied as a separate
  transfer that preserves asset conservation (section 9.1).

## 3. Markets and parameters

A **market** is an ordered pair `(base, quote)` of two distinct token contracts. The base token is
what is bought and sold. The quote token is the currency each price is denominated in. Native XLM
is referenced by its Stellar Asset Contract address, like every other token.

Each market has the following parameters, set once at creation from trusted configuration and
immutable afterwards:

| Parameter | Type | Meaning |
|-----------|------|---------|
| `lot_size` | `u64`, `> 0` | Base atoms per lot. All order sizes are whole lots. |
| `tick_size` | `u64`, `> 0` | Price increment, scaled by `PRICE_SCALE` (section 4.1). |

Constants shared by all markets:

| Constant | Value | Meaning |
|----------|-------|---------|
| `PRICE_SCALE` | `10^8` | Denominator for scaled prices. |
| `MAX_TICK` | implementation constant | Largest valid tick index. |
| `MAX_MATCH_STEPS` | implementation constant | Work budget per transaction (section 10). |

## 4. Units, price, and rounding

### 4.1 Units and price

- **Atoms** are the smallest unit of a token. Base amounts are measured in base atoms and quote
  amounts in quote atoms.
- **Lots** are the unit of order size. `base_atoms = base_lots * lot_size`. This conversion is
  exact and checked.
- A **tick** is a price index in `1..=MAX_TICK`. Ticks are the only prices an order can carry.
- The **scaled price** is `scaled_price = tick * tick_size`, computed with checked `u64`
  multiplication. An order whose scaled price overflows is rejected.
- The **price** is the number of quote atoms per base atom, `scaled_price / PRICE_SCALE`.

For a base amount `b` in atoms at scaled price `s`, the quote amount is:

```text
quote(b, s) = b * s / PRICE_SCALE            (exact rational value)
```

All intermediate products use checked `i128` arithmetic. An overflow rejects the operation. It never
wraps and never saturates.

### 4.2 Rounding

Every division in the protocol takes an explicit rounding mode. There is no default. Two functions
are used:

- `Up(x)` is the ceiling of the exact rational value `x`.
- `Down(x)` is the floor of the exact rational value `x`.

The rules below are binding. They are chosen so that every token atom is accounted for.

| Quantity | Rounding | Reason |
|----------|----------|--------|
| Quote locked by a bid for a remaining amount `r` lots | `Up(quote(r * lot_size, s))` | The trader never under-reserves. |
| Base locked by an ask for a remaining amount `r` lots | exact, `r * lot_size` | Base is never divided. |
| Quote paid to the seller for a fill of `f` lots | `Down(quote(f * lot_size, s))` | The protocol never over-pays a seller. |
| Quote debited from the buyer for a fill | difference of two `Up` locks (section 7.2) | Buyer debit always covers seller credit. |

The difference between the buyer's debit and the seller's credit is a **rounding residue**. It is
less than one quote atom per fill. The residue is credited to the market's protocol dust account
(section 9.1), so no value is created or destroyed.

## 5. Orders

### 5.1 Fields

Each order has:

| Field | Type | Meaning |
|-------|------|---------|
| `id` | `u64` | Per-market sequence number. Strictly increasing and never reused. This is the order's time priority. |
| `owner` | `Address` | The account that placed the order. |
| `side` | `Bid` or `Ask` | `Bid` buys base with quote. `Ask` sells base for quote. |
| `tick` | `u64` | Limit price. |
| `remaining_lots` | `u64` | Unfilled size. Always `> 0` while the order rests. |
| `locked` | `i128` | Tokens currently locked for this order (section 5.2). |
| `tif` | `GTC` or `GTD(expiry_ledger)` | Time in force (section 5.3). |

### 5.2 Locking

An order locks its full notional before it is matched.

- A bid with `remaining_lots = r` locks `Up(quote(r * lot_size, scaled_price))` quote atoms.
- An ask with `remaining_lots = r` locks `r * lot_size` base atoms.

Locks are taken from the trader's available balance. The lock is **all or nothing**. If the
available balance is insufficient, the order is rejected and nothing changes. A partially locked
order never exists.

A bid's `locked` is always equal to `Up(quote(remaining_lots * lot_size, scaled_price))`. This
equality is the basis for the fill debit in section 7.2.

### 5.3 Time in force

- **GTC** orders rest until they are fully filled or cancelled.
- **GTD(expiry_ledger)** orders rest until `expiry_ledger`. They are inactive when the current
  ledger sequence is greater than or equal to `expiry_ledger`.

A GTD order MUST be placed with `expiry_ledger > current_ledger`. A GTC order carries no expiry.

### 5.4 Placement procedure

A new order is processed in this order:

1. Validate: `lots >= 1`, `tick` in range, `tif` valid, and the caller is authorized as `owner`.
2. Lock the full notional (section 5.2). Reject on insufficient balance.
3. Match against the opposite side of the book (section 6).
4. Rest the remainder only if all of the following hold:
   - `remaining_lots > 0`,
   - the order is active under its time in force,
   - matching ended because no crossing liquidity was left, not because of a bound or a self-trade
     (sections 6.9 and 10).
5. Otherwise release the remainder's lock to the owner's available balance.

A transaction creates at most one resting order: the taker's remainder.

## 6. Matching

### 6.1 Priority

An incoming order is the **taker**. An order already resting on the book is a **maker**.

- **Price priority**: the best price matches first. For an incoming bid this is the lowest ask tick.
  For an incoming ask it is the highest bid tick.
- **Time priority**: within one tick, the lowest `id` matches first. Each tick is a FIFO queue.

Priority is total. Two resting orders never have the same `(tick, id)`.

### 6.2 Crossing

- An incoming bid at tick `t` crosses a resting ask at tick `q` if and only if `t >= q`.
- An incoming ask at tick `t` crosses a resting bid at tick `p` if and only if `t <= p`.

Matching stops at the first resting order that does not cross.

### 6.3 Execution price

Every fill executes at the **maker's tick**. The taker receives any price improvement. For example,
an incoming bid at tick 12 that matches a resting ask at tick 10 executes at tick 10.

### 6.4 Fill size and partial fills

The fill size is `f = min(taker.remaining_lots, maker.remaining_lots)`.

- A partially filled maker keeps its `id`, so it keeps its place in the queue.
- A partially filled taker continues matching against the next crossing maker.
- A maker whose `remaining_lots` reaches zero is removed from the book immediately. A zero-size
  order never rests.

### 6.5 Equal prices

Orders at the same tick are filled in ascending `id` order. A later order at that tick never
overtakes an earlier one. Equal-price orders are not merged.

### 6.6 Zero remainder

When the taker's `remaining_lots` reaches zero:

- no order rests,
- the taker's lock ends at `Up(quote(0, s)) = 0` for a bid, so no quote is released,
- any remaining base lock for an ask is zero, because the ask locked exactly its base.

No zero-size order is ever stored, emitted as resting, or returned as active.

### 6.7 Matching loop

Matching repeats the following until one of the stop conditions holds:

1. Select the highest-priority maker on the opposite side (section 6.1).
2. If there is none, or it does not cross the taker (section 6.2), stop with **no liquidity**.
3. If the maker belongs to the same owner as the taker, stop with **self-trade** (section 6.9).
4. If the maker is expired (section 8.2), remove it and continue.
5. Otherwise fill `f` lots at the maker's tick (section 7 settles it), and continue.

Each iteration counts against the work budget (section 10).

### 6.8 Expired makers

A resting GTD maker whose expiry has passed is inactive. It MUST NOT be filled. Matching removes
it, releases its lock to its owner, and continues. Each removal counts as one step of the work
budget.

### 6.9 Self-trade

If the next crossing maker belongs to the taker's owner, matching stops and the taker's remainder
is released. The maker is not touched and no fill is produced. This is **cancel-newest**: the
incoming order yields to the trader's own resting order. The stop emits a `SelfTradeStop` event.

## 7. Settlement

### 7.1 Settlement timing

Fills settle **immediately**, within the transaction that produces them. There is no pending
settlement state in v1. Every fill moves tokens between locked and available balances of the
accounts involved.

### 7.2 Fill legs

For a fill of `f` lots at scaled price `s` between a bid `B` and an ask `A`, let `b = f * lot_size`:

**Base leg.** The seller's locked base decreases by `b`. The buyer's available base increases by
`b`.

**Quote leg.**

```text
lock_before = Up(quote(remaining_before(B) * lot_size, s))
lock_after  = Up(quote(remaining_after(B)  * lot_size, s))
debit       = lock_before - lock_after              (taken from B's locked quote)
credit      = Down(quote(b, s))                     (added to A's available quote)
residue     = debit - credit                        (added to the protocol dust account)
```

`remaining_before(B) = remaining_after(B) + f`. The buyer's locked quote is reduced by `debit`,
and the buyer's `locked` field is updated to `lock_after`.

**Residue is never negative.** For real numbers `x, y >= 0`, `ceil(x + y) >= ceil(x) + floor(y)`.
Set `x = quote(remaining_after(B) * lot_size, s)` and `y = quote(b, s)`. Then `ceil(x) = lock_after`,
`floor(y) = credit`, and `ceil(x + y) = lock_before`. So `residue >= 0`.

Because each debit is a difference of consecutive locks, the debits of a bid plus the release of its
final lock (on cancel or expiry) add up to the lock it took in section 5.2. No fill can debit more
than the bid has locked.

### 7.3 Ask-side accounting

An ask's lock is exact base atoms, so its fill reduces `locked` by `b` and no quote rounding applies
on the base side. Its quote proceeds are the credit in section 7.2.

## 8. Cancellation and expiry

### 8.1 Cancellation

The owner of an order MAY cancel it at any time, with the owner's authorization. Cancellation:

- removes the order from the book,
- releases its remaining lock to the owner's available balance (quote for a bid, base for an ask),
- does not reuse the order's `id`,
- has no effect on any other order.

An order is cancelled entirely or not at all. No partial cancellation exists in v1.

### 8.2 Expiry

A GTD order is **expired** once the current ledger sequence is greater than or equal to its
`expiry_ledger`. Expiry is evaluated lazily:

- matching removes expired makers it encounters (section 6.8),
- any caller MAY remove an expired order directly. The order's lock is released to its owner, not to
  the caller.

Removing an expired order emits an event, and the removal does not change any fill.

---

*Sections 9 to 11 (invariants, bounded matching, and open decisions) follow in the second part of
this specification.*
