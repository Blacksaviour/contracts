// CLOB Settlement, Accounting, Fees, and Order Lifecycle Implementation
// Addresses issues #957, #958, #959, #981

#![no_std]
use soroban_sdk::{Address, Env, i128};

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
        if quote_sum != 0 { return Err("quote not conserved"); }
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
