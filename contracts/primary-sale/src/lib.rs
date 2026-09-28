//! Primary sale escrow for one Kastor property on Soroban.
//!
//! Port of `KastorEscrowMultiple` / `KastorEscrowV2` (BSC). A verified
//! investor pays USDC into escrow and the property units are reserved for
//! them. After the holding period (legal paperwork, contract signature) the
//! operator either completes the purchase, which sends USDC to the treasury
//! and the units to the investor, or refunds it.
//!
//! Fixes over the Solidity version:
//! - `completePurchase` sent the NFTs to the caller (the operator) instead
//!   of the buyer. Here units always go to the buyer.
//! - `reversePurchase` refunded without deleting the purchase, so it could
//!   be refunded twice. Here the purchase is removed before paying.
//! - Units are reserved at `buy`, so the same unit cannot be sold twice while
//!   a purchase sits in escrow.
//! - If the operator never acts, the buyer can take the money back after
//!   `holding + refund_grace` ledgers.
//! - Price is fixed in USDC. The BUSD/Chainlink oracle conversion is gone
//!   because the payment asset is already dollar-denominated.
#![no_std]

use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype,
    panic_with_error, token::TokenClient, Address, Env,
};

const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_THRESHOLD: u32 = INSTANCE_BUMP - DAY_IN_LEDGERS;
const ENTRY_BUMP: u32 = 120 * DAY_IN_LEDGERS;
const ENTRY_THRESHOLD: u32 = ENTRY_BUMP - 7 * DAY_IN_LEDGERS;

#[contractclient(name = "RegistryClient")]
pub trait Registry {
    fn is_verified(env: Env, wallet: Address) -> bool;
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub admin: Address,
    pub property_token: Address,
    pub registry: Address,
    pub usdc: Address,
    pub treasury: Address,
    /// USDC (in its smallest unit) per property unit.
    pub unit_price: i128,
    /// Ledgers a purchase must stay in escrow before it can be completed.
    pub holding_ledgers: u32,
    /// Extra ledgers after the holding period before the buyer may
    /// self-refund an unprocessed purchase.
    pub refund_grace_ledgers: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Purchase {
    pub units: i128,
    pub usdc: i128,
    /// Ledger of the latest `buy`; topping up restarts the holding period.
    pub ledger: u32,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Config,
    Reserved,
    Paused,
    Purchase(Address),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    InvalidAmount = 1,
    Paused = 2,
    NotVerified = 3,
    NotEnoughUnits = 4,
    NoPurchase = 5,
    HoldingPeriod = 6,
    RefundNotYetAllowed = 7,
    Overflow = 8,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Purchased {
    #[topic]
    pub buyer: Address,
    pub units: i128,
    pub usdc: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completed {
    #[topic]
    pub buyer: Address,
    pub units: i128,
    pub usdc: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Refunded {
    #[topic]
    pub buyer: Address,
    pub usdc: i128,
    pub by_buyer: bool,
}

#[contract]
pub struct PrimarySale;

#[contractimpl]
impl PrimarySale {
    pub fn __constructor(env: Env, config: Config) {
        if config.unit_price <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::Reserved, &0i128);
    }

    /// Pays `units * unit_price` USDC into escrow and reserves the units.
    pub fn buy(env: Env, buyer: Address, units: i128) {
        buyer.require_auth();
        let cfg = read_config(&env);
        if units <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        if is_paused(&env) {
            panic_with_error!(&env, Error::Paused);
        }
        if !RegistryClient::new(&env, &cfg.registry).is_verified(&buyer) {
            panic_with_error!(&env, Error::NotVerified);
        }
        if units > available(&env, &cfg) {
            panic_with_error!(&env, Error::NotEnoughUnits);
        }
        let cost = cfg
            .unit_price
            .checked_mul(units)
            .unwrap_or_else(|| panic_with_error!(&env, Error::Overflow));

        TokenClient::new(&env, &cfg.usdc).transfer(&buyer, env.current_contract_address(), &cost);

        let mut purchase = read_purchase(&env, &buyer).unwrap_or(Purchase {
            units: 0,
            usdc: 0,
            ledger: 0,
        });
        purchase.units += units;
        purchase.usdc += cost;
        purchase.ledger = env.ledger().sequence();
        write_purchase(&env, &buyer, &purchase);
        set_reserved(&env, reserved(&env) + units);

        Purchased {
            buyer,
            units,
            usdc: cost,
        }
        .publish(&env);
    }

    /// Settles a purchase once the holding period is over.
    pub fn complete(env: Env, buyer: Address) {
        let cfg = read_config(&env);
        cfg.admin.require_auth();
        let purchase = take_purchase(&env, &buyer);
        if env.ledger().sequence() < purchase.ledger + cfg.holding_ledgers {
            panic_with_error!(&env, Error::HoldingPeriod);
        }
        set_reserved(&env, reserved(&env) - purchase.units);

        let this = env.current_contract_address();
        TokenClient::new(&env, &cfg.usdc).transfer(&this, &cfg.treasury, &purchase.usdc);
        TokenClient::new(&env, &cfg.property_token).transfer(&this, &buyer, &purchase.units);

        Completed {
            buyer,
            units: purchase.units,
            usdc: purchase.usdc,
        }
        .publish(&env);
    }

    /// Operator-initiated refund (failed paperwork, KYC revoked, etc.).
    pub fn refund(env: Env, buyer: Address) {
        let cfg = read_config(&env);
        cfg.admin.require_auth();
        let purchase = take_purchase(&env, &buyer);
        pay_refund(&env, &cfg, &buyer, &purchase, false);
    }

    /// Buyer takes the money back if the operator never settled the purchase.
    pub fn claim_refund(env: Env, buyer: Address) {
        buyer.require_auth();
        let cfg = read_config(&env);
        let purchase = take_purchase(&env, &buyer);
        let allowed_from = purchase.ledger + cfg.holding_ledgers + cfg.refund_grace_ledgers;
        if env.ledger().sequence() < allowed_from {
            panic_with_error!(&env, Error::RefundNotYetAllowed);
        }
        pay_refund(&env, &cfg, &buyer, &purchase, true);
    }

    /// Returns unsold (unreserved) units to `to`, e.g. when the sale closes.
    pub fn withdraw_unsold(env: Env, to: Address, units: i128) {
        let cfg = read_config(&env);
        cfg.admin.require_auth();
        if units <= 0 || units > available(&env, &cfg) {
            panic_with_error!(&env, Error::NotEnoughUnits);
        }
        TokenClient::new(&env, &cfg.property_token).transfer(
            &env.current_contract_address(),
            &to,
            &units,
        );
    }

    pub fn set_unit_price(env: Env, unit_price: i128) {
        let mut cfg = read_config(&env);
        cfg.admin.require_auth();
        if unit_price <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        cfg.unit_price = unit_price;
        env.storage().instance().set(&DataKey::Config, &cfg);
        bump_instance(&env);
    }

    pub fn set_paused(env: Env, paused: bool) {
        read_config(&env).admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &paused);
        bump_instance(&env);
    }

    // ---------------------------------------------------------------- views

    pub fn config(env: Env) -> Config {
        read_config(&env)
    }

    pub fn purchase(env: Env, buyer: Address) -> Option<Purchase> {
        read_purchase(&env, &buyer)
    }

    /// Units that can still be bought.
    pub fn available(env: Env) -> i128 {
        available(&env, &read_config(&env))
    }

    pub fn reserved(env: Env) -> i128 {
        reserved(&env)
    }
}

fn read_config(env: &Env) -> Config {
    env.storage().instance().get(&DataKey::Config).unwrap()
}

fn is_paused(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&DataKey::Paused)
        .unwrap_or(false)
}

fn reserved(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::Reserved)
        .unwrap_or(0)
}

fn set_reserved(env: &Env, value: i128) {
    env.storage().instance().set(&DataKey::Reserved, &value);
    bump_instance(env);
}

fn available(env: &Env, cfg: &Config) -> i128 {
    let inventory =
        TokenClient::new(env, &cfg.property_token).balance(&env.current_contract_address());
    inventory - reserved(env)
}

fn read_purchase(env: &Env, buyer: &Address) -> Option<Purchase> {
    env.storage()
        .persistent()
        .get(&DataKey::Purchase(buyer.clone()))
}

fn write_purchase(env: &Env, buyer: &Address, purchase: &Purchase) {
    let key = DataKey::Purchase(buyer.clone());
    env.storage().persistent().set(&key, purchase);
    env.storage()
        .persistent()
        .extend_ttl(&key, ENTRY_THRESHOLD, ENTRY_BUMP);
}

/// Reads and deletes the purchase, so no path can settle it twice.
fn take_purchase(env: &Env, buyer: &Address) -> Purchase {
    let key = DataKey::Purchase(buyer.clone());
    let purchase: Purchase = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| panic_with_error!(env, Error::NoPurchase));
    env.storage().persistent().remove(&key);
    purchase
}

fn pay_refund(env: &Env, cfg: &Config, buyer: &Address, purchase: &Purchase, by_buyer: bool) {
    set_reserved(env, reserved(env) - purchase.units);
    TokenClient::new(env, &cfg.usdc).transfer(
        &env.current_contract_address(),
        buyer,
        &purchase.usdc,
    );
    Refunded {
        buyer: buyer.clone(),
        usdc: purchase.usdc,
        by_buyer,
    }
    .publish(env);
}

fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_THRESHOLD, INSTANCE_BUMP);
}

#[cfg(test)]
mod test;
