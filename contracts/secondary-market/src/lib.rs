//! Secondary market for Kastor property tokens on Soroban.
//!
//! The whitepaper's secondary market: a holder lists some of their units at a
//! price, and another verified investor buys them, paying in USDC. The buyer
//! goes through the same KYC as in the primary sale.
//!
//! There was no secondary market contract on BSC; this one is new.
//!
//! Design:
//! - Non-custodial. The seller keeps the units until someone buys them; the
//!   market moves them with `transfer_from` using the seller's allowance. Units
//!   held by the market would earn the seller's rental income in `dividends`,
//!   which pays pro-rata to balances, so the market never holds units.
//! - Only property tokens enabled by the operator can be listed, so the
//!   marketplace cannot be used to sell look-alike tokens.
//! - Partial fills. A listing can be bought in several purchases.
//! - The KYC check runs here (clear errors) and again inside the token, so a
//!   seller whose verification expired cannot sell even with an old listing.
//! - A fee in basis points goes to the treasury, capped at `MAX_FEE_BPS`.
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

/// 10 %. The fee is set by the operator and can never exceed this.
pub const MAX_FEE_BPS: u32 = 1_000;
const BPS: i128 = 10_000;

#[contractclient(name = "RegistryClient")]
pub trait Registry {
    fn is_verified(env: Env, wallet: Address) -> bool;
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub admin: Address,
    pub registry: Address,
    pub usdc: Address,
    pub treasury: Address,
    pub fee_bps: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Listing {
    pub seller: Address,
    pub property: Address,
    /// Units still for sale.
    pub units: i128,
    /// USDC (in its smallest unit) per property unit.
    pub unit_price: i128,
    /// Last ledger at which the listing can be bought.
    pub expiration_ledger: u32,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Config,
    Paused,
    NextId,
    Property(Address),
    Listing(u64),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    InvalidAmount = 1,
    Paused = 2,
    NotVerified = 3,
    PropertyNotEnabled = 4,
    NoListing = 5,
    ListingExpired = 6,
    NotEnoughUnits = 7,
    SellerBalanceTooLow = 8,
    AllowanceTooLow = 9,
    SelfTrade = 10,
    FeeTooHigh = 11,
    Overflow = 12,
    InvalidExpiration = 13,
    NotSeller = 14,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Listed {
    #[topic]
    pub listing_id: u64,
    #[topic]
    pub property: Address,
    pub seller: Address,
    pub units: i128,
    pub unit_price: i128,
    pub expiration_ledger: u32,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sold {
    #[topic]
    pub listing_id: u64,
    #[topic]
    pub property: Address,
    pub seller: Address,
    pub buyer: Address,
    pub units: i128,
    pub paid: i128,
    pub fee: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cancelled {
    #[topic]
    pub listing_id: u64,
    pub by_admin: bool,
}

#[contract]
pub struct SecondaryMarket;

#[contractimpl]
impl SecondaryMarket {
    pub fn __constructor(env: Env, config: Config) {
        if config.fee_bps > MAX_FEE_BPS {
            panic_with_error!(&env, Error::FeeTooHigh);
        }
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::NextId, &0u64);
    }

    /// Lists `units` of `property` at `unit_price` USDC each until
    /// `expiration_ledger`. The seller must also `approve` this contract on the
    /// property token for at least the units listed.
    pub fn list(
        env: Env,
        seller: Address,
        property: Address,
        units: i128,
        unit_price: i128,
        expiration_ledger: u32,
    ) -> u64 {
        seller.require_auth();
        let cfg = read_config(&env);
        if units <= 0 || unit_price <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        if is_paused(&env) {
            panic_with_error!(&env, Error::Paused);
        }
        if !is_enabled(&env, &property) {
            panic_with_error!(&env, Error::PropertyNotEnabled);
        }
        if expiration_ledger < env.ledger().sequence() {
            panic_with_error!(&env, Error::InvalidExpiration);
        }
        require_verified(&env, &cfg, &seller);
        if TokenClient::new(&env, &property).balance(&seller) < units {
            panic_with_error!(&env, Error::SellerBalanceTooLow);
        }
        // Cost of buying the whole listing must fit in i128.
        unit_price
            .checked_mul(units)
            .unwrap_or_else(|| panic_with_error!(&env, Error::Overflow));

        let id: u64 = env.storage().instance().get(&DataKey::NextId).unwrap();
        env.storage().instance().set(&DataKey::NextId, &(id + 1));
        bump_instance(&env);

        let listing = Listing {
            seller: seller.clone(),
            property: property.clone(),
            units,
            unit_price,
            expiration_ledger,
        };
        write_listing(&env, id, &listing);

        Listed {
            listing_id: id,
            property,
            seller,
            units,
            unit_price,
            expiration_ledger,
        }
        .publish(&env);
        id
    }

    /// Buys `units` from a listing. The buyer pays `units * unit_price` USDC:
    /// the fee to the treasury and the rest to the seller. The units move from
    /// the seller to the buyer in the same transaction.
    pub fn buy(env: Env, buyer: Address, listing_id: u64, units: i128) {
        buyer.require_auth();
        let cfg = read_config(&env);
        if units <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        if is_paused(&env) {
            panic_with_error!(&env, Error::Paused);
        }
        let mut listing = read_listing(&env, listing_id);
        if env.ledger().sequence() > listing.expiration_ledger {
            panic_with_error!(&env, Error::ListingExpired);
        }
        if !is_enabled(&env, &listing.property) {
            panic_with_error!(&env, Error::PropertyNotEnabled);
        }
        if buyer == listing.seller {
            panic_with_error!(&env, Error::SelfTrade);
        }
        if units > listing.units {
            panic_with_error!(&env, Error::NotEnoughUnits);
        }
        require_verified(&env, &cfg, &buyer);
        require_verified(&env, &cfg, &listing.seller);

        let property = TokenClient::new(&env, &listing.property);
        let this = env.current_contract_address();
        if property.balance(&listing.seller) < units {
            panic_with_error!(&env, Error::SellerBalanceTooLow);
        }
        if property.allowance(&listing.seller, &this) < units {
            panic_with_error!(&env, Error::AllowanceTooLow);
        }

        let cost = listing
            .unit_price
            .checked_mul(units)
            .unwrap_or_else(|| panic_with_error!(&env, Error::Overflow));
        let fee = cost
            .checked_mul(cfg.fee_bps as i128)
            .unwrap_or_else(|| panic_with_error!(&env, Error::Overflow))
            / BPS;

        // Effects before interactions: the listing is updated first.
        listing.units -= units;
        if listing.units == 0 {
            env.storage()
                .persistent()
                .remove(&DataKey::Listing(listing_id));
        } else {
            write_listing(&env, listing_id, &listing);
        }

        let usdc = TokenClient::new(&env, &cfg.usdc);
        usdc.transfer(&buyer, &listing.seller, &(cost - fee));
        if fee > 0 {
            usdc.transfer(&buyer, &cfg.treasury, &fee);
        }
        property.transfer_from(&this, &listing.seller, &buyer, &units);

        Sold {
            listing_id,
            property: listing.property,
            seller: listing.seller,
            buyer,
            units,
            paid: cost,
            fee,
        }
        .publish(&env);
    }

    /// The seller withdraws a listing.
    pub fn cancel(env: Env, seller: Address, listing_id: u64) {
        seller.require_auth();
        let listing = read_listing(&env, listing_id);
        if listing.seller != seller {
            panic_with_error!(&env, Error::NotSeller);
        }
        remove_listing(&env, listing_id, false);
    }

    /// The operator withdraws a listing, e.g. when the seller's KYC is revoked.
    pub fn admin_cancel(env: Env, listing_id: u64) {
        read_config(&env).admin.require_auth();
        read_listing(&env, listing_id);
        remove_listing(&env, listing_id, true);
    }

    pub fn set_property(env: Env, property: Address, enabled: bool) {
        read_config(&env).admin.require_auth();
        let key = DataKey::Property(property);
        if enabled {
            env.storage().persistent().set(&key, &true);
            env.storage()
                .persistent()
                .extend_ttl(&key, ENTRY_THRESHOLD, ENTRY_BUMP);
        } else {
            env.storage().persistent().remove(&key);
        }
    }

    pub fn set_fee(env: Env, fee_bps: u32) {
        let mut cfg = read_config(&env);
        cfg.admin.require_auth();
        if fee_bps > MAX_FEE_BPS {
            panic_with_error!(&env, Error::FeeTooHigh);
        }
        cfg.fee_bps = fee_bps;
        write_config(&env, &cfg);
    }

    pub fn set_treasury(env: Env, treasury: Address) {
        let mut cfg = read_config(&env);
        cfg.admin.require_auth();
        cfg.treasury = treasury;
        write_config(&env, &cfg);
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

    pub fn listing(env: Env, listing_id: u64) -> Option<Listing> {
        env.storage()
            .persistent()
            .get(&DataKey::Listing(listing_id))
    }

    pub fn is_property_enabled(env: Env, property: Address) -> bool {
        is_enabled(&env, &property)
    }

    pub fn is_paused(env: Env) -> bool {
        is_paused(&env)
    }
}

fn read_config(env: &Env) -> Config {
    env.storage().instance().get(&DataKey::Config).unwrap()
}

fn write_config(env: &Env, cfg: &Config) {
    env.storage().instance().set(&DataKey::Config, cfg);
    bump_instance(env);
}

fn is_paused(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&DataKey::Paused)
        .unwrap_or(false)
}

fn is_enabled(env: &Env, property: &Address) -> bool {
    env.storage()
        .persistent()
        .get(&DataKey::Property(property.clone()))
        .unwrap_or(false)
}

fn require_verified(env: &Env, cfg: &Config, wallet: &Address) {
    if !RegistryClient::new(env, &cfg.registry).is_verified(wallet) {
        panic_with_error!(env, Error::NotVerified);
    }
}

fn read_listing(env: &Env, id: u64) -> Listing {
    env.storage()
        .persistent()
        .get(&DataKey::Listing(id))
        .unwrap_or_else(|| panic_with_error!(env, Error::NoListing))
}

fn write_listing(env: &Env, id: u64, listing: &Listing) {
    let key = DataKey::Listing(id);
    env.storage().persistent().set(&key, listing);
    env.storage()
        .persistent()
        .extend_ttl(&key, ENTRY_THRESHOLD, ENTRY_BUMP);
}

fn remove_listing(env: &Env, id: u64, by_admin: bool) {
    env.storage().persistent().remove(&DataKey::Listing(id));
    Cancelled {
        listing_id: id,
        by_admin,
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
