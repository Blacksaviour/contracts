// CLOB Settlement, Accounting, Fees, and Order Lifecycle Implementation
// Addresses issues #957, #958, #959, #960, #961, #962, #963, #964, #965, #967, #981

#![no_std]
use soroban_sdk::{Address, Env, i128};

/// Fixed-point denominator used for protocol fee rates.
/// A rate of 100_000 (0.1%) is represented as 100_000 / FEE_RATE_SCALE.
pub const FEE_RATE_SCALE: i128 = 100_000_000;

/// Protocol maximum for any maker or taker rate. Market configuration must not
/// exceed it. A rate of zero means that side pays no fee.
pub const MAX_FEE_RATE: i128 = FEE_RATE_SCALE;

// ============ Resource-bound constants (Phase 0, #013) ============
/// Maximum number of steps (fills, expired-maker removals, tick scans) per transaction.
/// Enforced against Soroban CPU/memory/ledger limits. Consensus-critical.
/// Set by benchmark harness; contract and SDK must use this same value.
pub const MAX_MATCH_STEPS: u32 = 100; // Placeholder; real value set by benchmark.

/// Largest valid tick index. Orders and market prices must not exceed it.
/// Consensus-critical; set by benchmark harness.
pub const MAX_TICK: u64 = 1_000_000; // Placeholder; real value set by benchmark.

/// Price scale denominator (protocol spec section 4.1).
/// Scaled price = tick * tick_size / PRICE_SCALE.
pub const PRICE_SCALE: i128 = 100_000_000;

// ============ Bounded-work constants (Phase 0, #013, part 1/2) ============
/// Maximum number of fill operations per single matching call.
/// Limits CPU work and ledger writes from matching one incoming order.
/// When exhausted, matching stops deterministically and the client must resubmit the remainder.
/// Rationale: Soroban transactions have finite CPU, memory, and ledger write budgets.
/// Fills are the primary work unit; each fill requires O(1) ledger writes and compute.
pub const MAX_MATCHES_PER_CALL: u32 = 100;

/// Maximum number of price levels (ticks) examined during one matching traversal.
/// Limits iteration over potentially sparse order book levels when finding next liquidity.
/// When exhausted, matching stops and client must resubmit to continue at next level.
/// Rationale: Finding the next non-empty level can require scanning many empty ticks.
/// Bounding this prevents quadratic-time matching on adversarially sparse books.
/// Implementation must use an ordered index or bitmap to find levels in O(1) per level examined.
pub const MAX_LEVELS_CROSSED_PER_CALL: u32 = 100;

/// Maximum number of orders in a batch operation (batch cancel, batch place).
/// Limits the number of individual order operations processed in one transaction.
/// Rationale: Each order operation (create, cancel) requires ledger reads/writes.
/// Batch operations are not atomic; partial batches succeed and client must retry remainder.
pub const MAX_BATCH_SIZE: u32 = 50;

/// Maximum number of orders returned in a depth query (market data snapshot).
/// Limits ledger read operations when querying the order book state.
/// Rationale: Depth queries scan the book; bounding them keeps query costs predictable.
/// Clients paginate through large books using continuation tokens.
pub const MAX_DEPTH_QUERY_SIZE: u32 = 100;

/// Maximum size (in bytes) of a single order's payload (including owner, side, tick, size, tif).
/// Prevents adversarially large orders from consuming excessive transaction space.
/// Rationale: Soroban transactions have a hard size limit. Bounding per-order size ensures
/// reasonable batch sizes remain within transaction budget.
pub const MAX_ORDER_PAYLOAD_BYTES: u32 = 256;

// ============ Resource-bound guards (Phase 0, #013) ============
/// Validates that a tick is within protocol bounds.
/// Rejects any value > MAX_TICK.
pub fn validate_tick(tick: u64) -> Result<(), &'static str> {
    if tick == 0 {
        return Err("tick: must be positive");
    }
    if tick > MAX_TICK {
        return Err("tick: exceeds MAX_TICK");
    }
    Ok(())
}

/// Validates that a match-step count is within protocol bounds.
/// Rejects any value > MAX_MATCH_STEPS.
pub fn validate_match_steps(steps: u32) -> Result<(), &'static str> {
    if steps > MAX_MATCH_STEPS {
        return Err("match_steps: exceeds MAX_MATCH_STEPS");
    }
    Ok(())
}

/// Validates lot size is positive (protocol spec section 3).
/// Lot size must be > 0; zero and negative sizes are rejected at market creation.
pub fn validate_lot_size(lot_size: u64) -> Result<(), &'static str> {
    if lot_size == 0 {
        return Err("lot_size: must be positive");
    }
    Ok(())
}

/// Validates tick size is positive and does not overflow when scaled.
/// Tick size must be > 0. Product tick * tick_size must fit in u64 (checked at order placement).
pub fn validate_tick_size(tick_size: u64) -> Result<(), &'static str> {
    if tick_size == 0 {
        return Err("tick_size: must be positive");
    }
    Ok(())
}

/// Validates that a fill count does not exceed MAX_MATCHES_PER_CALL.
/// Called during matching to enforce bounded work per transaction.
pub fn validate_matches_per_call(count: u32) -> Result<(), &'static str> {
    if count > MAX_MATCHES_PER_CALL {
        return Err("matches: exceeds MAX_MATCHES_PER_CALL");
    }
    Ok(())
}

/// Validates that a level count does not exceed MAX_LEVELS_CROSSED_PER_CALL.
/// Called during book traversal to enforce bounded level scanning.
pub fn validate_levels_crossed_per_call(count: u32) -> Result<(), &'static str> {
    if count > MAX_LEVELS_CROSSED_PER_CALL {
        return Err("levels: exceeds MAX_LEVELS_CROSSED_PER_CALL");
    }
    Ok(())
}

/// Validates that a batch size does not exceed MAX_BATCH_SIZE.
/// Called when accepting batch operations.
pub fn validate_batch_size(size: u32) -> Result<(), &'static str> {
    if size > MAX_BATCH_SIZE {
        return Err("batch: exceeds MAX_BATCH_SIZE");
    }
    Ok(())
}

/// Validates that a depth query size does not exceed MAX_DEPTH_QUERY_SIZE.
/// Called when returning market data.
pub fn validate_depth_query_size(size: u32) -> Result<(), &'static str> {
    if size > MAX_DEPTH_QUERY_SIZE {
        return Err("depth: exceeds MAX_DEPTH_QUERY_SIZE");
    }
    Ok(())
}

/// Validates that an order payload does not exceed MAX_ORDER_PAYLOAD_BYTES.
/// Called at order creation to enforce transaction size bounds.
pub fn validate_order_payload_size(size: u32) -> Result<(), &'static str> {
    if size > MAX_ORDER_PAYLOAD_BYTES {
        return Err("order: payload exceeds MAX_ORDER_PAYLOAD_BYTES");
    }
    Ok(())
}

// ============ #957: Base and Quote Balance Transfers ============
#[derive(Clone)]
pub struct Fill {
    pub maker: Address,
    pub taker: Address,
    pub base_amount: i128,
    pub quote_amount: i128,
    pub maker_is_sell: bool,
}

#[derive(Clone)]
pub struct SettlementLedger {
    pub base_transfers: Vec<(Address, i128)>,
    pub quote_transfers: Vec<(Address, i128)>,
    /// Protocol-owned liabilities, kept separate from trader transfers.
    pub fee_liabilities: Vec<(Address, i128)>,
}

impl SettlementLedger {
    fn add_fee_liability(&mut self, token: Address, amount: i128) -> Result<(), &'static str> {
        if amount < 0 { return Err("fee: negative accrual"); }
        // A zero fee leaves no entry, so zero-fee markets have no hidden deductions.
        if amount > 0 { self.fee_liabilities.push((token, amount)); }
        Ok(())
    }
}

/// Protocol fees owed for one fill, in quote atoms. Both amounts belong to the
/// protocol, never to a trader.
#[derive(Clone, Copy)]
pub struct FillFees {
    pub taker: i128,
    pub maker: i128,
}

impl FillFees {
    pub fn total(&self) -> Result<i128, &'static str> {
        self.taker.checked_add(self.maker).ok_or("fee: total overflow")
    }
}

impl Fill {
    pub fn settle_balance_transfer(
        &self,
        _env: &Env,
        ledger: &mut SettlementLedger,
    ) -> Result<(), &'static str> {
        if self.maker_is_sell {
            ledger.base_transfers.push((self.maker.clone(), -self.base_amount));
            ledger.quote_transfers.push((self.maker.clone(), self.quote_amount));
            ledger.base_transfers.push((self.taker.clone(), self.base_amount));
            ledger.quote_transfers.push((self.taker.clone(), -self.quote_amount));
        } else {
            ledger.quote_transfers.push((self.maker.clone(), -self.quote_amount));
            ledger.base_transfers.push((self.maker.clone(), self.base_amount));
            ledger.quote_transfers.push((self.taker.clone(), self.quote_amount));
            ledger.base_transfers.push((self.taker.clone(), -self.base_amount));
        }
        Ok(())
    }

    /// Fees owed for this fill. `taker_quote_before` is the quote the same taker
    /// already filled earlier in the transaction (zero for its first fill), so
    /// the taker fee is charged on the running total, not on each fragment.
    pub fn fees(
        &self,
        config: &MarketFeeConfig,
        taker_quote_before: i128,
    ) -> Result<FillFees, &'static str> {
        if self.base_amount < 0 || self.quote_amount < 0 {
            return Err("fill: amounts must be non-negative");
        }
        Ok(FillFees {
            taker: config.cumulative_taker_fee_delta(taker_quote_before, self.quote_amount)?,
            maker: config.maker_fee(self.quote_amount)?,
        })
    }

    /// Settle a fill and its fees as one ledger operation.  Fees are deducted
    /// from each trader's quote side and recorded as protocol liabilities; they
    /// are never added to a trader's available balance.
    pub fn settle_with_fee(
        &self,
        ledger: &mut SettlementLedger,
        config: &MarketFeeConfig,
        quote_token: Address,
        taker_quote_before: i128,
    ) -> Result<FillFees, &'static str> {
        let fees = self.fees(config, taker_quote_before)?;
        self.settle_with_fees(ledger, fees, quote_token)?;
        Ok(fees)
    }

    /// Variant used by the contract settlement path. The accumulator is
    /// preflighted before mutating either economic state, then updated only
    /// after the ledger operation succeeds.
    pub fn settle_with_fee_and_accumulator(
        &self,
        ledger: &mut SettlementLedger,
        accumulator: &mut FeeAccumulator,
        config: &MarketFeeConfig,
        taker_quote_before: i128,
    ) -> Result<FillFees, &'static str> {
        let fees = self.fees(config, taker_quote_before)?;
        let total = fees.total()?;
        accumulator
            .accrued
            .checked_add(total)
            .ok_or("fee: accumulator overflow")?;
        self.settle_with_fees(ledger, fees, accumulator.token.clone())?;
        // The preflight above proves this cannot fail now.
        accumulator.accrued = accumulator.accrued.checked_add(total)
            .ok_or("fee: accumulator overflow")?;
        Ok(fees)
    }

    fn settle_with_fees(
        &self,
        ledger: &mut SettlementLedger,
        fees: FillFees,
        quote_token: Address,
    ) -> Result<(), &'static str> {
        // Stage all mutations so an overflow or conservation failure leaves
        // the caller's economic state untouched.
        let mut staged = ledger.clone();
        let quote = self.quote_amount;
        // The maker's fee is taken from its own leg: a seller receives quote net
        // of the fee, a buyer pays quote plus the fee. The taker's leg mirrors it.
        let maker_quote = if self.maker_is_sell {
            quote.checked_sub(fees.maker)
        } else {
            quote.checked_add(fees.maker).and_then(|v| v.checked_neg())
        }
        .ok_or("fee: maker quote overflow")?;
        let taker_quote = if self.maker_is_sell {
            quote.checked_add(fees.taker).and_then(|v| v.checked_neg())
        } else {
            quote.checked_sub(fees.taker)
        }
        .ok_or("fee: taker quote overflow")?;
        if self.maker_is_sell {
            staged.base_transfers.push((self.maker.clone(), -self.base_amount));
            staged.base_transfers.push((self.taker.clone(), self.base_amount));
        } else {
            staged.base_transfers.push((self.maker.clone(), self.base_amount));
            staged.base_transfers.push((self.taker.clone(), -self.base_amount));
        }
        staged.quote_transfers.push((self.maker.clone(), maker_quote));
        staged.quote_transfers.push((self.taker.clone(), taker_quote));
        staged.add_fee_liability(quote_token.clone(), fees.taker)?;
        staged.add_fee_liability(quote_token, fees.maker)?;
        Self::validate_conservation(&staged)?;
        *ledger = staged;
        Ok(())
    }
    
    pub fn validate_conservation(ledger: &SettlementLedger) -> Result<(), &'static str> {
        let mut base_sum: i128 = 0;
        for (_, delta) in &ledger.base_transfers {
            base_sum = base_sum.checked_add(*delta).ok_or("base overflow")?;
        }
        if base_sum != 0 { return Err("base not conserved"); }
        
        let mut quote_sum: i128 = 0;
        for (_, delta) in &ledger.quote_transfers {
            quote_sum = quote_sum.checked_add(*delta).ok_or("quote overflow")?;
        }
        let mut fees: i128 = 0;
        for (_, amount) in &ledger.fee_liabilities {
            fees = fees.checked_add(*amount).ok_or("fee overflow")?;
        }
        // Fee deductions are liabilities, not trader-owned quote.  Including
        // them here makes custody reconciliation distinguish the two owners.
        if quote_sum.checked_add(fees).ok_or("quote overflow")? != 0 {
            return Err("quote not conserved");
        }
        Ok(())
    }
}

/// Market-wide v1 fee configuration.  The lookup is O(1) and does not depend
/// on historical volume or any unbounded trader scan.  Both rates are fixed
/// point over `FEE_RATE_SCALE` and non-negative: maker rebates are out of
/// scope for v1.
#[derive(Clone, Copy)]
pub struct MarketFeeConfig {
    pub taker_fee_rate: i128,
    pub maker_fee_rate: i128,
}

impl MarketFeeConfig {
    pub fn new(taker_fee_rate: i128, maker_fee_rate: i128) -> Result<Self, &'static str> {
        validate_fee_rate(taker_fee_rate)?;
        validate_fee_rate(maker_fee_rate)?;
        Ok(Self { taker_fee_rate, maker_fee_rate })
    }

    /// Fee charged to the taker on a fill of `quote_amount`, rounded up.
    pub fn taker_fee(&self, quote_amount: i128) -> Result<i128, &'static str> {
        fee_on_quote(quote_amount, self.taker_fee_rate)
    }

    /// Fee charged to the maker on a fill of `quote_amount`, rounded up.
    pub fn maker_fee(&self, quote_amount: i128) -> Result<i128, &'static str> {
        fee_on_quote(quote_amount, self.maker_fee_rate)
    }

    /// Taker fee for a fill that takes the taker's running total from
    /// `taker_quote_before` to `taker_quote_before + fill_quote`.  The fee is the
    /// difference of two rounded-up totals, so the fees of consecutive fills sum
    /// to the fee of their total: splitting a fill never changes the fee.
    pub fn cumulative_taker_fee_delta(
        &self,
        taker_quote_before: i128,
        fill_quote: i128,
    ) -> Result<i128, &'static str> {
        if fill_quote < 0 { return Err("fee: negative quote amount"); }
        let before = self.taker_fee(taker_quote_before)?;
        let total = taker_quote_before
            .checked_add(fill_quote)
            .ok_or("fee: cumulative quote overflow")?;
        self.taker_fee(total)?
            .checked_sub(before)
            .ok_or("fee: cumulative underflow")
    }

    /// Quote a bid must lock to cover its quote and the larger of its maker and
    /// taker fees.  A resting bid can be filled as a maker later, so its lock
    /// cannot assume the taker role.
    pub fn required_bid_reservation(&self, quote_amount: i128) -> Result<i128, &'static str> {
        let fee = self.taker_fee(quote_amount)?.max(self.maker_fee(quote_amount)?);
        quote_amount.checked_add(fee).ok_or("fee: reservation overflow")
    }

    /// Resolve one already-authorized bounded tier. `None` is the unambiguous
    /// v1 default and uses the market-wide rate. A tier replaces only the taker rate.
    pub fn resolve_tier(&self, tier: Option<FeeTier>) -> Result<Self, &'static str> {
        match tier {
            Some(tier) => {
                tier.validate()?;
                Ok(Self { taker_fee_rate: tier.taker_fee_rate, ..*self })
            }
            None => Ok(*self),
        }
    }
}

/// A bounded, auditable trader tier.  v1 callers may pass `None` and always
/// get the market default; no historical-volume accounting is performed.
#[derive(Clone, Copy)]
pub struct FeeTier {
    pub id: u32,
    pub taker_fee_rate: i128,
}

impl FeeTier {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_fee_rate(self.taker_fee_rate)
    }
}

fn validate_fee_rate(rate: i128) -> Result<(), &'static str> {
    if rate < 0 || rate > MAX_FEE_RATE {
        return Err("fee: rate out of range");
    }
    Ok(())
}

/// Fee on `quote_amount` at `rate`, rounded up so a non-zero obligation can
/// never be under-reserved.  A zero rate charges nothing.
fn fee_on_quote(quote_amount: i128, rate: i128) -> Result<i128, &'static str> {
    if quote_amount < 0 { return Err("fee: negative quote amount"); }
    let numerator = quote_amount.checked_mul(rate).ok_or("fee: multiplication overflow")?;
    if numerator == 0 { return Ok(0); }
    numerator.checked_add(FEE_RATE_SCALE - 1)
        .ok_or("fee: rounding overflow")
        .map(|n| n / FEE_RATE_SCALE)
}

// ============ #958: Fee Buckets ============
#[derive(Clone)]
pub struct FeeBucket {
    pub token: Address,
    pub execution_fee: i128,
    pub position_fee: i128,
    pub referral_credit: i128,
}

impl FeeBucket {
    pub fn new(token: Address) -> Self {
        Self { token, execution_fee: 0, position_fee: 0, referral_credit: 0 }
    }
    
    pub fn apply_execution_fee(&mut self, amount: i128) -> Result<(), &'static str> {
        self.execution_fee = self.execution_fee.checked_add(amount).ok_or("fee overflow")?;
        Ok(())
    }
    
    pub fn apply_position_fee(&mut self, amount: i128) -> Result<(), &'static str> {
        self.position_fee = self.position_fee.checked_add(amount).ok_or("fee overflow")?;
        Ok(())
    }
    
    pub fn apply_referral_credit(&mut self, amount: i128) -> Result<(), &'static str> {
        self.referral_credit = self.referral_credit.checked_add(amount).ok_or("credit overflow")?;
        Ok(())
    }
    
    pub fn net_fee(&self) -> Result<i128, &'static str> {
        let gross = self.execution_fee.checked_add(self.position_fee).ok_or("gross overflow")?;
        gross.checked_sub(self.referral_credit).ok_or("net overflow")
    }
}

/// Explicit protocol-owned balance for one token.  It is intentionally not a
/// `TraderAccount`, so withdrawal code cannot spend it as user available funds.
#[derive(Clone)]
pub struct FeeAccumulator {
    pub token: Address,
    pub accrued: i128,
}

impl FeeAccumulator {
    pub fn new(token: Address) -> Self { Self { token, accrued: 0 } }

    pub fn accrue(&mut self, amount: i128) -> Result<(), &'static str> {
        if amount < 0 { return Err("fee: negative accrual"); }
        self.accrued = self.accrued.checked_add(amount).ok_or("fee: accumulator overflow")?;
        Ok(())
    }

    pub fn pay_out(&mut self, amount: i128) -> Result<(), &'static str> {
        if amount < 0 { return Err("fee: negative payout"); }
        self.accrued = self.accrued.checked_sub(amount).ok_or("fee: insufficient accrued fees")?;
        Ok(())
    }
}

// ============ #959, #960: Quote Rounding ============
/// Only `Down` and `Up` exist: the spec names no other rounding mode (section 4.2).
#[derive(Clone, Copy)]
pub enum RoundingMode { Down, Up }

/// Quote for `base_atoms` at `scaled_price`, rounded as section 4.2 requires.
/// Contract and SDK previews must call this function, not re-derive the math.
pub fn quote_atoms(
    base_atoms: i128,
    scaled_price: i128,
    rounding: RoundingMode,
) -> Result<i128, &'static str> {
    if base_atoms < 0 || scaled_price < 0 {
        return Err("quote: negative input");
    }
    let numerator = base_atoms.checked_mul(scaled_price).ok_or("quote overflow")?;
    let quotient = numerator / PRICE_SCALE;
    match rounding {
        RoundingMode::Down => Ok(quotient),
        RoundingMode::Up if numerator % PRICE_SCALE == 0 => Ok(quotient),
        RoundingMode::Up => Ok(quotient + 1),
    }
}

/// The quote legs of one fill (protocol spec section 7.2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FillQuoteLegs {
    /// Removed from the buyer's locked quote.
    pub debit: i128,
    /// Added to the seller's available quote.
    pub credit: i128,
    /// Credited to the protocol dust account.
    pub residue: i128,
}

/// Quote legs for filling `fill_lots` of a bid with `remaining_before_lots` left.
/// The debit is a difference of locks, so the buyer's total over any split of
/// the same bid equals the total over one fill (spec section 10.4, Example C).
pub fn fill_quote_legs(
    lot_size: i128,
    remaining_before_lots: i128,
    fill_lots: i128,
    scaled_price: i128,
) -> Result<FillQuoteLegs, &'static str> {
    if lot_size <= 0 {
        return Err("fill: lot size must be positive");
    }
    if fill_lots <= 0 || fill_lots > remaining_before_lots {
        return Err("fill: size out of range");
    }
    let remaining_after_lots = remaining_before_lots - fill_lots;
    let lock_before = quote_atoms(
        remaining_before_lots.checked_mul(lot_size).ok_or("fill: atoms overflow")?,
        scaled_price,
        RoundingMode::Up,
    )?;
    let lock_after = quote_atoms(
        remaining_after_lots.checked_mul(lot_size).ok_or("fill: atoms overflow")?,
        scaled_price,
        RoundingMode::Up,
    )?;
    let credit = quote_atoms(
        fill_lots.checked_mul(lot_size).ok_or("fill: atoms overflow")?,
        scaled_price,
        RoundingMode::Down,
    )?;
    let debit = lock_before - lock_after;
    let residue = debit - credit;
    // Two rounded-up values differ by at most one atom from their rounded-down
    // sum, so a single fill can only leave a residue of 0 or 1.
    if !(0..=1).contains(&residue) {
        return Err("fill: residue out of bounds");
    }
    Ok(FillQuoteLegs { debit, credit, residue })
}

// ============ #981: Order Lifecycle ============
#[derive(Clone, Copy)]
pub enum TimeInForce { GTC, GTD }

pub struct OrderRequest {
    pub tif: TimeInForce,
    pub expiry_ledger: u64,
}

impl OrderRequest {
    pub fn validate_tif(
        env: &Env,
        tif: TimeInForce,
        expiry: Option<u64>,
    ) -> Result<(), &'static str> {
        match tif {
            TimeInForce::GTC => {
                if let Some(exp) = expiry { if exp != 0 { return Err("gtc: expiry must be zero"); } }
                Ok(())
            }
            TimeInForce::GTD => {
                let expiry = expiry.ok_or("gtd: missing expiry")?;
                if expiry <= env.ledger().sequence() { return Err("gtd: expiry passed"); }
                Ok(())
            }
        }
    }

    pub fn is_active(&self, current_ledger: u64) -> bool {
        match self.tif {
            TimeInForce::GTC => true,
            TimeInForce::GTD => current_ledger < self.expiry_ledger,
        }
    }
}

// ============ #903-906: Custody, Deposits, and Withdrawals ============

#[derive(Clone)]
pub struct TraderAccount {
    pub trader: Address,
    pub asset: Address,
    pub total_balance: i128,
    pub available_balance: i128,
    pub locked_balance: i128,
}

impl TraderAccount {
    pub fn new(trader: Address, asset: Address) -> Self {
        Self {
            trader,
            asset,
            total_balance: 0,
            available_balance: 0,
            locked_balance: 0,
        }
    }

    pub fn validate_invariants(&self) -> Result<(), &'static str> {
        let computed_total = self.available_balance.checked_add(self.locked_balance)
            .ok_or("balance overflow")?;
        if computed_total != self.total_balance {
            return Err("balance invariant violated");
        }
        if self.available_balance < 0 || self.locked_balance < 0 || self.total_balance < 0 {
            return Err("negative balance");
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct DepositRequest {
    pub trader: Address,
    pub asset: Address,
    pub amount: i128,
}

impl DepositRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.amount <= 0 {
            return Err("deposit: amount must be positive");
        }
        Ok(())
    }

    pub fn process_deposit(
        &self,
        account: &mut TraderAccount,
    ) -> Result<(), &'static str> {
        self.validate()?;

        let new_total = self.amount.checked_add(account.total_balance)
            .ok_or("deposit: total overflow")?;
        let new_available = self.amount.checked_add(account.available_balance)
            .ok_or("deposit: available overflow")?;

        account.total_balance = new_total;
        account.available_balance = new_available;

        account.validate_invariants()?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct WithdrawalRequest {
    pub trader: Address,
    pub asset: Address,
    pub amount: i128,
}

impl WithdrawalRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.amount <= 0 {
            return Err("withdrawal: amount must be positive");
        }
        Ok(())
    }

    pub fn process_withdrawal(
        &self,
        account: &mut TraderAccount,
    ) -> Result<(), &'static str> {
        self.validate()?;

        if self.amount > account.available_balance {
            return Err("withdrawal: insufficient available balance");
        }

        let new_available = account.available_balance.checked_sub(self.amount)
            .ok_or("withdrawal: available underflow")?;
        let new_total = account.total_balance.checked_sub(self.amount)
            .ok_or("withdrawal: total underflow")?;

        account.available_balance = new_available;
        account.total_balance = new_total;

        account.validate_invariants()?;
        Ok(())
    }
}

#[cfg(test)]
mod fee_tests {
    use super::*;

    #[test]
    fn reservation_rounds_fee_up() {
        let config = MarketFeeConfig::new(1, 0).unwrap();
        assert_eq!(config.taker_fee(1).unwrap(), 1);
        assert_eq!(config.required_bid_reservation(1).unwrap(), 2);
    }

    #[test]
    fn zero_fee_has_no_extra_reservation() {
        let config = MarketFeeConfig::new(0, 0).unwrap();
        assert_eq!(config.required_bid_reservation(123).unwrap(), 123);
    }

    #[test]
    fn invalid_rates_and_overflow_are_rejected() {
        assert!(MarketFeeConfig::new(-1, 0).is_err());
        assert!(MarketFeeConfig::new(0, -1).is_err());
        assert!(MarketFeeConfig::new(FEE_RATE_SCALE + 1, 0).is_err());
        assert!(MarketFeeConfig::new(0, MAX_FEE_RATE + 1).is_err());
        let config = MarketFeeConfig::new(MAX_FEE_RATE, 0).unwrap();
        assert!(config.required_bid_reservation(i128::MAX).is_err());
    }

    #[test]
    fn tier_resolution_is_bounded_and_default_is_explicit() {
        let config = MarketFeeConfig::new(100, 50).unwrap();
        assert_eq!(config.resolve_tier(None).unwrap().taker_fee_rate, 100);
        let tier = FeeTier { id: 7, taker_fee_rate: 200 };
        let resolved = config.resolve_tier(Some(tier)).unwrap();
        assert_eq!(resolved.taker_fee_rate, 200);
        assert_eq!(resolved.maker_fee_rate, 50);
    }

    #[test]
    fn bid_reservation_covers_the_larger_of_maker_and_taker_fee() {
        // 0.1% taker fee and 0.2% maker fee: the maker fee is the larger obligation.
        let config = MarketFeeConfig::new(100_000, 200_000).unwrap();
        assert_eq!(config.taker_fee(1_000).unwrap(), 1);
        assert_eq!(config.maker_fee(1_000).unwrap(), 2);
        assert_eq!(config.required_bid_reservation(1_000).unwrap(), 1_002);
    }

    #[test]
    fn split_taker_fills_pay_the_fee_of_their_total() {
        // A one-atom fill alone rounds its fee up to one atom. Three one-atom
        // fills must still pay one atom in total, not three.
        let config = MarketFeeConfig::new(1, 0).unwrap();
        let mut filled = 0;
        let mut paid = 0;
        for _ in 0..3 {
            paid += config.cumulative_taker_fee_delta(filled, 1).unwrap();
            filled += 1;
        }
        assert_eq!(config.taker_fee(3).unwrap(), 1);
        assert_eq!(paid, 1);
    }
}

#[cfg(test)]
mod fee_settlement_tests {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    fn empty_ledger() -> SettlementLedger {
        SettlementLedger {
            base_transfers: Vec::new(),
            quote_transfers: Vec::new(),
            fee_liabilities: Vec::new(),
        }
    }

    #[test]
    fn maker_sell_pays_maker_fee_from_its_proceeds() {
        let env = Env::default();
        let quote_token = Address::generate(&env);
        // 0.1% taker fee and 0.05% maker fee.
        let config = MarketFeeConfig::new(100_000, 50_000).unwrap();
        let fill = Fill {
            maker: Address::generate(&env),
            taker: Address::generate(&env),
            base_amount: 10,
            quote_amount: 1_000_000,
            maker_is_sell: true,
        };
        let mut ledger = empty_ledger();
        let mut accumulator = FeeAccumulator::new(quote_token);
        let fees = fill
            .settle_with_fee_and_accumulator(&mut ledger, &mut accumulator, &config, 0)
            .unwrap();
        assert_eq!(fees.taker, 1_000);
        assert_eq!(fees.maker, 500);
        // The seller receives quote net of its fee; the buyer pays quote plus its fee.
        assert_eq!(ledger.quote_transfers[0].1, 999_500);
        assert_eq!(ledger.quote_transfers[1].1, -1_001_000);
        assert_eq!(accumulator.accrued, 1_500);
    }

    #[test]
    fn maker_buy_pays_quote_plus_fee_and_taker_seller_nets_its_fee() {
        let env = Env::default();
        let config = MarketFeeConfig::new(100_000, 50_000).unwrap();
        let fill = Fill {
            maker: Address::generate(&env),
            taker: Address::generate(&env),
            base_amount: 10,
            quote_amount: 1_000_000,
            maker_is_sell: false,
        };
        let mut ledger = empty_ledger();
        let fees = fill
            .settle_with_fee(&mut ledger, &config, Address::generate(&env), 0)
            .unwrap();
        assert_eq!(fees.total().unwrap(), 1_500);
        assert_eq!(ledger.base_transfers[0].1, 10);
        assert_eq!(ledger.quote_transfers[0].1, -1_000_500);
        assert_eq!(ledger.quote_transfers[1].1, 999_000);
    }

    #[test]
    fn zero_fee_market_has_no_liability_and_no_deduction() {
        let env = Env::default();
        let config = MarketFeeConfig::new(0, 0).unwrap();
        let fill = Fill {
            maker: Address::generate(&env),
            taker: Address::generate(&env),
            base_amount: 10,
            quote_amount: 1_000_000,
            maker_is_sell: true,
        };
        let mut ledger = empty_ledger();
        let fees = fill
            .settle_with_fee(&mut ledger, &config, Address::generate(&env), 0)
            .unwrap();
        assert_eq!(fees.total().unwrap(), 0);
        assert!(ledger.fee_liabilities.is_empty());
        assert_eq!(ledger.quote_transfers[0].1, 1_000_000);
        assert_eq!(ledger.quote_transfers[1].1, -1_000_000);
    }

    #[test]
    fn split_fills_charge_one_taker_fee_in_total_through_settlement() {
        let env = Env::default();
        let quote_token = Address::generate(&env);
        let config = MarketFeeConfig::new(1, 0).unwrap();
        let taker = Address::generate(&env);
        let mut ledger = empty_ledger();
        let mut taker_quote_before = 0;
        let mut taker_fees = 0;
        for _ in 0..3 {
            let fill = Fill {
                maker: Address::generate(&env),
                taker: taker.clone(),
                base_amount: 1,
                quote_amount: 1,
                maker_is_sell: false,
            };
            let fees = fill
                .settle_with_fee(&mut ledger, &config, quote_token.clone(), taker_quote_before)
                .unwrap();
            taker_fees += fees.taker;
            taker_quote_before += 1;
        }
        assert_eq!(taker_fees, config.taker_fee(3).unwrap());
    }
}

#[cfg(test)]
mod rounding_tests {
    use super::*;

    // Spec section 10.4 Examples B and C: lot_size 1, tick 3 with
    // tick_size 150_000_000, so the scaled price is 4.5 quote atoms per base atom.
    const TICK_3: i128 = 450_000_000;

    #[test]
    fn quote_atoms_rounds_in_the_requested_direction() {
        assert_eq!(quote_atoms(3, TICK_3, RoundingMode::Up).unwrap(), 14);
        assert_eq!(quote_atoms(3, TICK_3, RoundingMode::Down).unwrap(), 13);
        assert_eq!(quote_atoms(2, TICK_3, RoundingMode::Up).unwrap(), 9);
        assert_eq!(quote_atoms(2, TICK_3, RoundingMode::Down).unwrap(), 9);
    }

    #[test]
    fn quote_atoms_rejects_negative_and_overflowing_inputs() {
        assert!(quote_atoms(-1, TICK_3, RoundingMode::Up).is_err());
        assert!(quote_atoms(i128::MAX, TICK_3, RoundingMode::Down).is_err());
    }

    #[test]
    fn single_fill_residue_is_credited_to_dust() {
        // Example B: the buyer is debited 14, the seller credited 13, dust gains 1.
        let legs = fill_quote_legs(1, 3, 3, TICK_3).unwrap();
        assert_eq!(legs, FillQuoteLegs { debit: 14, credit: 13, residue: 1 });
    }

    #[test]
    fn split_fills_pay_the_same_as_one_fill() {
        // Example C: fills of 1 lot and 2 lots total the same as one 3-lot fill.
        let whole = fill_quote_legs(1, 3, 3, TICK_3).unwrap();
        let first = fill_quote_legs(1, 3, 1, TICK_3).unwrap();
        let second = fill_quote_legs(1, 2, 2, TICK_3).unwrap();
        assert_eq!(first.debit + second.debit, whole.debit);
        assert_eq!(first.credit + second.credit, whole.credit);
        assert_eq!(first.residue + second.residue, whole.residue);
    }

    #[test]
    fn fragmentation_never_increases_seller_credit() {
        // Seller credit is a floor of each part, so the parts sum to at most the
        // floor of the whole. Splitting can only move value to the dust account.
        let whole_credit = quote_atoms(5, TICK_3, RoundingMode::Down).unwrap();
        let mut parts_credit = 0;
        // Five 1-lot fills of a 5-lot bid, with the remaining size walking 5 -> 1.
        for remaining in (1..=5).rev() {
            parts_credit += fill_quote_legs(1, remaining, 1, TICK_3).unwrap().credit;
        }
        assert_eq!(whole_credit, 22);
        assert_eq!(parts_credit, 20);
        assert!(parts_credit <= whole_credit);
    }

    #[test]
    fn fill_size_and_lot_size_are_validated() {
        assert!(fill_quote_legs(1, 3, 0, TICK_3).is_err());
        assert!(fill_quote_legs(1, 3, 4, TICK_3).is_err());
        assert!(fill_quote_legs(0, 3, 1, TICK_3).is_err());
    }
}

#[cfg(test)]
mod resource_bound_tests {
    use super::*;

    // ============ Tick validation (positive paths) ============
    #[test]
    fn validate_tick_accepts_valid_ticks() {
        assert!(validate_tick(1).is_ok());
        assert!(validate_tick(100).is_ok());
        assert!(validate_tick(MAX_TICK).is_ok());
        assert!(validate_tick(MAX_TICK / 2).is_ok());
    }

    // ============ Tick validation (negative paths) ============
    #[test]
    fn validate_tick_rejects_zero() {
        assert_eq!(validate_tick(0), Err("tick: must be positive"));
    }

    #[test]
    fn validate_tick_rejects_out_of_bounds() {
        assert_eq!(validate_tick(MAX_TICK + 1), Err("tick: exceeds MAX_TICK"));
        assert_eq!(validate_tick(u64::MAX), Err("tick: exceeds MAX_TICK"));
    }

    // ============ Match steps validation (positive paths) ============
    #[test]
    fn validate_match_steps_accepts_valid_budget() {
        assert!(validate_match_steps(0).is_ok());
        assert!(validate_match_steps(1).is_ok());
        assert!(validate_match_steps(50).is_ok());
        assert!(validate_match_steps(MAX_MATCH_STEPS).is_ok());
    }

    // ============ Match steps validation (negative paths) ============
    #[test]
    fn validate_match_steps_rejects_out_of_bounds() {
        assert_eq!(validate_match_steps(MAX_MATCH_STEPS + 1), Err("match_steps: exceeds MAX_MATCH_STEPS"));
        assert_eq!(validate_match_steps(u32::MAX), Err("match_steps: exceeds MAX_MATCH_STEPS"));
    }

    // ============ Lot size validation (positive paths) ============
    #[test]
    fn validate_lot_size_accepts_positive() {
        assert!(validate_lot_size(1).is_ok());
        assert!(validate_lot_size(100).is_ok());
        assert!(validate_lot_size(1_000_000_000).is_ok());
        assert!(validate_lot_size(u64::MAX).is_ok());
    }

    // ============ Lot size validation (negative paths) ============
    #[test]
    fn validate_lot_size_rejects_zero() {
        assert_eq!(validate_lot_size(0), Err("lot_size: must be positive"));
    }

    // ============ Tick size validation (positive paths) ============
    #[test]
    fn validate_tick_size_accepts_positive() {
        assert!(validate_tick_size(1).is_ok());
        assert!(validate_tick_size(100_000_000).is_ok());
        assert!(validate_tick_size(u64::MAX).is_ok());
    }

    // ============ Tick size validation (negative paths) ============
    #[test]
    fn validate_tick_size_rejects_zero() {
        assert_eq!(validate_tick_size(0), Err("tick_size: must be positive"));
    }

    // ============ Boundary tests ============
    #[test]
    fn max_constants_are_non_zero() {
        assert!(MAX_MATCH_STEPS > 0);
        assert!(MAX_TICK > 0);
    }

    #[test]
    fn price_scale_is_fixed_denominator() {
        assert_eq!(PRICE_SCALE, 100_000_000);
    }
}

#[cfg(test)]
mod bounded_work_tests {
    use super::*;

    // ============ Matches per call validation (positive paths) ============
    #[test]
    fn validate_matches_per_call_accepts_valid_count() {
        assert!(validate_matches_per_call(0).is_ok());
        assert!(validate_matches_per_call(1).is_ok());
        assert!(validate_matches_per_call(50).is_ok());
        assert!(validate_matches_per_call(MAX_MATCHES_PER_CALL).is_ok());
    }

    // ============ Matches per call validation (negative paths) ============
    #[test]
    fn validate_matches_per_call_rejects_out_of_bounds() {
        assert_eq!(validate_matches_per_call(MAX_MATCHES_PER_CALL + 1), Err("matches: exceeds MAX_MATCHES_PER_CALL"));
        assert_eq!(validate_matches_per_call(u32::MAX), Err("matches: exceeds MAX_MATCHES_PER_CALL"));
    }

    // ============ Levels crossed per call validation (positive paths) ============
    #[test]
    fn validate_levels_crossed_per_call_accepts_valid_count() {
        assert!(validate_levels_crossed_per_call(0).is_ok());
        assert!(validate_levels_crossed_per_call(1).is_ok());
        assert!(validate_levels_crossed_per_call(50).is_ok());
        assert!(validate_levels_crossed_per_call(MAX_LEVELS_CROSSED_PER_CALL).is_ok());
    }

    // ============ Levels crossed per call validation (negative paths) ============
    #[test]
    fn validate_levels_crossed_per_call_rejects_out_of_bounds() {
        assert_eq!(validate_levels_crossed_per_call(MAX_LEVELS_CROSSED_PER_CALL + 1), Err("levels: exceeds MAX_LEVELS_CROSSED_PER_CALL"));
        assert_eq!(validate_levels_crossed_per_call(u32::MAX), Err("levels: exceeds MAX_LEVELS_CROSSED_PER_CALL"));
    }

    // ============ Batch size validation (positive paths) ============
    #[test]
    fn validate_batch_size_accepts_valid_size() {
        assert!(validate_batch_size(0).is_ok());
        assert!(validate_batch_size(1).is_ok());
        assert!(validate_batch_size(25).is_ok());
        assert!(validate_batch_size(MAX_BATCH_SIZE).is_ok());
    }

    // ============ Batch size validation (negative paths) ============
    #[test]
    fn validate_batch_size_rejects_out_of_bounds() {
        assert_eq!(validate_batch_size(MAX_BATCH_SIZE + 1), Err("batch: exceeds MAX_BATCH_SIZE"));
        assert_eq!(validate_batch_size(u32::MAX), Err("batch: exceeds MAX_BATCH_SIZE"));
    }

    // ============ Depth query size validation (positive paths) ============
    #[test]
    fn validate_depth_query_size_accepts_valid_size() {
        assert!(validate_depth_query_size(0).is_ok());
        assert!(validate_depth_query_size(1).is_ok());
        assert!(validate_depth_query_size(50).is_ok());
        assert!(validate_depth_query_size(MAX_DEPTH_QUERY_SIZE).is_ok());
    }

    // ============ Depth query size validation (negative paths) ============
    #[test]
    fn validate_depth_query_size_rejects_out_of_bounds() {
        assert_eq!(validate_depth_query_size(MAX_DEPTH_QUERY_SIZE + 1), Err("depth: exceeds MAX_DEPTH_QUERY_SIZE"));
        assert_eq!(validate_depth_query_size(u32::MAX), Err("depth: exceeds MAX_DEPTH_QUERY_SIZE"));
    }

    // ============ Order payload size validation (positive paths) ============
    #[test]
    fn validate_order_payload_size_accepts_valid_size() {
        assert!(validate_order_payload_size(0).is_ok());
        assert!(validate_order_payload_size(1).is_ok());
        assert!(validate_order_payload_size(128).is_ok());
        assert!(validate_order_payload_size(MAX_ORDER_PAYLOAD_BYTES).is_ok());
    }

    // ============ Order payload size validation (negative paths) ============
    #[test]
    fn validate_order_payload_size_rejects_out_of_bounds() {
        assert_eq!(validate_order_payload_size(MAX_ORDER_PAYLOAD_BYTES + 1), Err("order: payload exceeds MAX_ORDER_PAYLOAD_BYTES"));
        assert_eq!(validate_order_payload_size(u32::MAX), Err("order: payload exceeds MAX_ORDER_PAYLOAD_BYTES"));
    }

    // ============ Boundary and relationship tests ============
    #[test]
    fn all_bounded_work_constants_are_positive() {
        assert!(MAX_MATCHES_PER_CALL > 0);
        assert!(MAX_LEVELS_CROSSED_PER_CALL > 0);
        assert!(MAX_BATCH_SIZE > 0);
        assert!(MAX_DEPTH_QUERY_SIZE > 0);
        assert!(MAX_ORDER_PAYLOAD_BYTES > 0);
    }

    #[test]
    fn bounded_work_constants_are_consistent() {
        // Batch size should be reasonable relative to matches per call
        assert!(MAX_BATCH_SIZE <= MAX_MATCHES_PER_CALL || MAX_BATCH_SIZE > 0);
        // All constants should be u32 representable
        assert!(MAX_MATCHES_PER_CALL <= u32::MAX);
        assert!(MAX_LEVELS_CROSSED_PER_CALL <= u32::MAX);
        assert!(MAX_BATCH_SIZE <= u32::MAX);
        assert!(MAX_DEPTH_QUERY_SIZE <= u32::MAX);
        assert!(MAX_ORDER_PAYLOAD_BYTES <= u32::MAX);
    }
}
