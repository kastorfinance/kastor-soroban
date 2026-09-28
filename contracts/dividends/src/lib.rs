//! Rental income (dividends) for Kastor property holders on Soroban.
//!
//! Port of `KastorRoyalties` (BSC). The operator allocates USDC to holders
//! and each holder claims it from their wallet.
//!
//! Fixes over the Solidity version:
//! - `claim()` never reset the claimed balance, so a holder could call it
//!   again and again and drain the source wallet. Here the balance is zeroed
//!   before paying.
//! - Allocations are funded up front: the USDC moves into this contract in
//!   the same call that credits the holders, so every claimable amount is
//!   backed. The BSC version pulled from an external wallet at claim time,
//!   which failed if that wallet's allowance or balance ran out.
//!
//! Added:
//! - `allocate_pro_rata` computes each share on-chain from the property
//!   token balances, instead of trusting amounts computed off-chain.
#![no_std]

use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype,
    panic_with_error, token::TokenClient, Address, Env, Map, Vec,
};

const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_THRESHOLD: u32 = INSTANCE_BUMP - DAY_IN_LEDGERS;
const ENTRY_BUMP: u32 = 120 * DAY_IN_LEDGERS;
const ENTRY_THRESHOLD: u32 = ENTRY_BUMP - 7 * DAY_IN_LEDGERS;

#[contractclient(name = "PropertyTokenClient")]
pub trait PropertyTokenView {
    fn balance(env: Env, id: Address) -> i128;
    fn total_supply(env: Env) -> i128;
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    Usdc,
    PropertyToken,
    Paused,
    Claimable(Address),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    InvalidInput = 1,
    NothingToClaim = 2,
    Paused = 3,
    DuplicateHolder = 4,
    NoSupply = 5,
    Overflow = 6,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Allocated {
    #[topic]
    pub holder: Address,
    pub amount: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Claimed {
    #[topic]
    pub holder: Address,
    pub amount: i128,
}

#[contract]
pub struct Dividends;

#[contractimpl]
impl Dividends {
    pub fn __constructor(env: Env, admin: Address, usdc: Address, property_token: Address) {
        let s = env.storage().instance();
        s.set(&DataKey::Admin, &admin);
        s.set(&DataKey::Usdc, &usdc);
        s.set(&DataKey::PropertyToken, &property_token);
    }

    /// Credits explicit amounts (`loadBalances` in the BSC version) and pulls
    /// their sum from the admin in the same call.
    pub fn allocate(env: Env, holders: Vec<Address>, amounts: Vec<i128>) -> i128 {
        let admin = read_admin(&env);
        admin.require_auth();
        if holders.is_empty() || holders.len() != amounts.len() {
            panic_with_error!(&env, Error::InvalidInput);
        }
        let mut total: i128 = 0;
        for i in 0..holders.len() {
            let amount = amounts.get_unchecked(i);
            if amount <= 0 {
                panic_with_error!(&env, Error::InvalidInput);
            }
            total = total
                .checked_add(amount)
                .unwrap_or_else(|| panic_with_error!(&env, Error::Overflow));
            credit(&env, &holders.get_unchecked(i), amount);
        }
        fund(&env, &admin, total);
        total
    }

    /// Splits `amount` USDC among `holders` in proportion to their current
    /// property token balance over total supply. Rounding dust stays with the
    /// admin (only the exact sum of the shares is pulled). Returns that sum.
    ///
    /// Balances are read at call time: the operator should pause the token
    /// while building the holder list so it matches the record date.
    pub fn allocate_pro_rata(env: Env, holders: Vec<Address>, amount: i128) -> i128 {
        let admin = read_admin(&env);
        admin.require_auth();
        if holders.is_empty() || amount <= 0 {
            panic_with_error!(&env, Error::InvalidInput);
        }
        let token_id: Address = env
            .storage()
            .instance()
            .get(&DataKey::PropertyToken)
            .unwrap();
        let token = PropertyTokenClient::new(&env, &token_id);
        let supply = token.total_supply();
        if supply <= 0 {
            panic_with_error!(&env, Error::NoSupply);
        }

        let mut seen: Map<Address, ()> = Map::new(&env);
        let mut total: i128 = 0;
        for holder in holders.iter() {
            if seen.contains_key(holder.clone()) {
                panic_with_error!(&env, Error::DuplicateHolder);
            }
            seen.set(holder.clone(), ());
            let share = amount
                .checked_mul(token.balance(&holder))
                .unwrap_or_else(|| panic_with_error!(&env, Error::Overflow))
                / supply;
            if share > 0 {
                credit(&env, &holder, share);
                total += share;
            }
        }
        if total > 0 {
            fund(&env, &admin, total);
        }
        total
    }

    pub fn claim(env: Env, holder: Address) -> i128 {
        holder.require_auth();
        if is_paused(&env) {
            panic_with_error!(&env, Error::Paused);
        }
        let key = DataKey::Claimable(holder.clone());
        let amount: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if amount <= 0 {
            panic_with_error!(&env, Error::NothingToClaim);
        }
        // Zero the balance before paying (the BSC version never did).
        env.storage().persistent().remove(&key);
        TokenClient::new(&env, &read_usdc(&env)).transfer(
            &env.current_contract_address(),
            &holder,
            &amount,
        );
        Claimed { holder, amount }.publish(&env);
        amount
    }

    pub fn claimable(env: Env, holder: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Claimable(holder))
            .unwrap_or(0)
    }

    pub fn set_paused(env: Env, paused: bool) {
        read_admin(&env).require_auth();
        env.storage().instance().set(&DataKey::Paused, &paused);
        bump_instance(&env);
    }

    pub fn admin(env: Env) -> Address {
        read_admin(&env)
    }
}

fn read_admin(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Admin).unwrap()
}

fn read_usdc(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Usdc).unwrap()
}

fn is_paused(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&DataKey::Paused)
        .unwrap_or(false)
}

fn credit(env: &Env, holder: &Address, amount: i128) {
    let key = DataKey::Claimable(holder.clone());
    let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
    let updated = current
        .checked_add(amount)
        .unwrap_or_else(|| panic_with_error!(env, Error::Overflow));
    env.storage().persistent().set(&key, &updated);
    env.storage()
        .persistent()
        .extend_ttl(&key, ENTRY_THRESHOLD, ENTRY_BUMP);
    Allocated {
        holder: holder.clone(),
        amount,
    }
    .publish(env);
}

fn fund(env: &Env, admin: &Address, total: i128) {
    TokenClient::new(env, &read_usdc(env)).transfer(admin, env.current_contract_address(), &total);
    bump_instance(env);
}

fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_THRESHOLD, INSTANCE_BUMP);
}

#[cfg(test)]
mod test;
