// CLOB Settlement, Accounting, Fees, and Order Lifecycle Implementation
// Addresses issues #957, #958, #959, #962, #964, #965, #967, #981

#![no_std]
use soroban_sdk::{Address, Env, i128};

/// Fixed-point denominator used for protocol fee rates.
/// A rate of 100_000 (0.1%) is represented as 100_000 / FEE_RATE_SCALE.
pub const FEE_RATE_SCALE: i128 = 100_000_000;

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
        self.fee_liabilities.push((token, amount));
        Ok(())
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

    /// Settle a fill and its taker fee as one ledger operation.  The fee is
    /// deducted from the taker's quote side and recorded as a protocol
    /// liability; it is never added to a trader's available balance.
    pub fn settle_with_fee(
        &self,
        ledger: &mut SettlementLedger,
        config: &MarketFeeConfig,
        quote_token: Address,
    ) -> Result<i128, &'static str> {
        if self.base_amount < 0 || self.quote_amount < 0 {
            return Err("fill: amounts must be non-negative");
        }
        let fee = config.taker_fee(self.quote_amount)?;
        // Stage all mutations so an overflow or conservation failure leaves
        // the caller's economic state untouched.
        let mut staged = ledger.clone();
        if self.maker_is_sell {
            staged.base_transfers.push((self.maker.clone(), -self.base_amount));
            staged.quote_transfers.push((self.maker.clone(), self.quote_amount));
            staged.base_transfers.push((self.taker.clone(), self.base_amount));
            let taker_quote = self.quote_amount.checked_add(fee).ok_or("fee: quote overflow")?;
            staged.quote_transfers.push((self.taker.clone(), -taker_quote));
        } else {
            staged.quote_transfers.push((self.maker.clone(), -self.quote_amount));
            staged.base_transfers.push((self.maker.clone(), self.base_amount));
            staged.quote_transfers.push((self.taker.clone(), self.quote_amount.checked_sub(fee)
                .ok_or("fee: quote underflow")?));
            staged.base_transfers.push((self.taker.clone(), -self.base_amount));
        }
        staged.add_fee_liability(quote_token, fee)?;
        Self::validate_conservation(&staged)?;
        *ledger = staged;
        Ok(fee)
    }

    /// Variant used by the contract settlement path. The accumulator is
    /// preflighted before mutating either economic state, then updated only
    /// after the ledger operation succeeds.
    pub fn settle_with_fee_and_accumulator(
        &self,
        ledger: &mut SettlementLedger,
        accumulator: &mut FeeAccumulator,
        config: &MarketFeeConfig,
    ) -> Result<i128, &'static str> {
        let fee = config.taker_fee(self.quote_amount)?;
        accumulator
            .accrued
            .checked_add(fee)
            .ok_or("fee: accumulator overflow")?;
        let settled_fee = self.settle_with_fee(ledger, config, accumulator.token.clone())?;
        // The preflight above proves this cannot fail now.
        accumulator.accrued = accumulator.accrued.checked_add(settled_fee)
            .ok_or("fee: accumulator overflow")?;
        Ok(settled_fee)
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
/// on historical volume or any unbounded trader scan.
#[derive(Clone, Copy)]
pub struct MarketFeeConfig {
    pub taker_fee_rate: i128,
}

impl MarketFeeConfig {
    pub fn new(taker_fee_rate: i128) -> Result<Self, &'static str> {
        if taker_fee_rate < 0 || taker_fee_rate > FEE_RATE_SCALE {
            return Err("fee: rate out of range");
        }
        Ok(Self { taker_fee_rate })
    }

    pub fn taker_fee(&self, quote_amount: i128) -> Result<i128, &'static str> {
        if quote_amount < 0 { return Err("fee: negative quote amount"); }
        let numerator = quote_amount.checked_mul(self.taker_fee_rate)
            .ok_or("fee: multiplication overflow")?;
        // Fees round up so a non-zero obligation can never be under-reserved.
        if numerator == 0 { return Ok(0); }
        numerator.checked_add(FEE_RATE_SCALE - 1)
            .ok_or("fee: rounding overflow")
            .map(|n| n / FEE_RATE_SCALE)
    }

    pub fn required_bid_reservation(&self, quote_amount: i128) -> Result<i128, &'static str> {
        let fee = self.taker_fee(quote_amount)?;
        quote_amount.checked_add(fee).ok_or("fee: reservation overflow")
    }

    /// Resolve one already-authorized bounded tier. `None` is the unambiguous
    /// v1 default and uses the market-wide rate.
    pub fn resolve_tier(&self, tier: Option<FeeTier>) -> Result<Self, &'static str> {
        match tier {
            Some(tier) => {
                tier.validate()?;
                Ok(Self { taker_fee_rate: tier.taker_fee_rate })
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
        if self.taker_fee_rate < 0 || self.taker_fee_rate > FEE_RATE_SCALE {
            return Err("fee: tier rate out of range");
        }
        Ok(())
    }
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

// ============ #959: Quote Rounding ============
#[derive(Clone, Copy)]
pub enum RoundingMode { Down, Up, Nearest }

pub struct QuoteCalculator {
    pub price: i128,
    pub base_amount: i128,
}

impl QuoteCalculator {
    pub fn calculate_quote(&self, rounding: RoundingMode) -> Result<i128, &'static str> {
        let numerator = self.base_amount.checked_mul(self.price).ok_or("quote overflow")?;
        let denominator = 100_000_000i128;
        
        match rounding {
            RoundingMode::Down => Ok(numerator / denominator),
            RoundingMode::Up => {
                let adj = numerator.checked_add(denominator - 1).ok_or("overflow")?;
                Ok(adj / denominator)
            }
            RoundingMode::Nearest => {
                let adj = numerator.checked_add(denominator / 2).ok_or("overflow")?;
                Ok(adj / denominator)
            }
        }
    }
    
    pub fn calculate_dust(&self, rounding: RoundingMode) -> Result<i128, &'static str> {
        let numerator = self.base_amount.checked_mul(self.price).ok_or("overflow")?;
        let denominator = 100_000_000i128;
        let quote = self.calculate_quote(rounding)?;
        let reconstructed = quote.checked_mul(denominator).ok_or("overflow")?;
        numerator.checked_sub(reconstructed).ok_or("dust negative")
    }
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
        let config = MarketFeeConfig::new(1).unwrap();
        assert_eq!(config.taker_fee(1).unwrap(), 1);
        assert_eq!(config.required_bid_reservation(1).unwrap(), 2);
    }

    #[test]
    fn zero_fee_has_no_extra_reservation() {
        let config = MarketFeeConfig::new(0).unwrap();
        assert_eq!(config.required_bid_reservation(123).unwrap(), 123);
    }

    #[test]
    fn invalid_rates_and_overflow_are_rejected() {
        assert!(MarketFeeConfig::new(-1).is_err());
        assert!(MarketFeeConfig::new(FEE_RATE_SCALE + 1).is_err());
        let config = MarketFeeConfig::new(FEE_RATE_SCALE).unwrap();
        assert!(config.required_bid_reservation(i128::MAX).is_err());
    }

    #[test]
    fn tier_resolution_is_bounded_and_default_is_explicit() {
        let config = MarketFeeConfig::new(100).unwrap();
        assert_eq!(config.resolve_tier(None).unwrap().taker_fee_rate, 100);
        let tier = FeeTier { id: 7, taker_fee_rate: 200 };
        assert_eq!(config.resolve_tier(Some(tier)).unwrap().taker_fee_rate, 200);
    }
}
