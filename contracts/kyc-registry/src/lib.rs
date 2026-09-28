//! KYC registry for Kastor Finance on Soroban.
//!
//! Port of `WhitelistVault` (BSC). The admin appoints operators (the KYC
//! backend) that mark wallets as verified after the off-chain KYC/AML check.
//! Changes from the Solidity version:
//! - Each verification carries an expiry ledger, so a stale KYC stops
//!   passing without anyone having to remove it.
//! - The global "freeway mode" (every address passes) was dropped: it turned
//!   off compliance for all properties at once with a single call.
#![no_std]

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, panic_with_error, Address,
    Env, Vec,
};

const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_THRESHOLD: u32 = INSTANCE_BUMP - DAY_IN_LEDGERS;
const ENTRY_BUMP: u32 = 120 * DAY_IN_LEDGERS;
const ENTRY_THRESHOLD: u32 = ENTRY_BUMP - 7 * DAY_IN_LEDGERS;

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    Operator(Address),
    /// Ledger sequence until which the wallet counts as verified (inclusive).
    Verified(Address),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotOperator = 1,
    ExpiryInPast = 2,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Verified {
    #[topic]
    pub wallet: Address,
    pub expires_at_ledger: u32,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Revoked {
    #[topic]
    pub wallet: Address,
}

#[contract]
pub struct KycRegistry;

#[contractimpl]
impl KycRegistry {
    pub fn __constructor(env: Env, admin: Address) {
        env.storage().instance().set(&DataKey::Admin, &admin);
    }

    pub fn admin(env: Env) -> Address {
        read_admin(&env)
    }

    pub fn set_admin(env: Env, new_admin: Address) {
        read_admin(&env).require_auth();
        new_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        bump_instance(&env);
    }

    pub fn set_operator(env: Env, operator: Address, enabled: bool) {
        read_admin(&env).require_auth();
        let key = DataKey::Operator(operator);
        if enabled {
            env.storage().persistent().set(&key, &true);
            env.storage()
                .persistent()
                .extend_ttl(&key, ENTRY_THRESHOLD, ENTRY_BUMP);
        } else {
            env.storage().persistent().remove(&key);
        }
        bump_instance(&env);
    }

    pub fn is_operator(env: Env, operator: Address) -> bool {
        env.storage().persistent().has(&DataKey::Operator(operator))
    }

    /// Marks `wallets` as verified until `expires_at_ledger` (inclusive).
    /// Re-verifying a wallet overwrites its previous expiry.
    pub fn verify(env: Env, operator: Address, wallets: Vec<Address>, expires_at_ledger: u32) {
        require_operator(&env, &operator);
        if expires_at_ledger < env.ledger().sequence() {
            panic_with_error!(&env, Error::ExpiryInPast);
        }
        for wallet in wallets.iter() {
            let key = DataKey::Verified(wallet.clone());
            env.storage().persistent().set(&key, &expires_at_ledger);
            env.storage()
                .persistent()
                .extend_ttl(&key, ENTRY_THRESHOLD, ENTRY_BUMP);
            Verified {
                wallet,
                expires_at_ledger,
            }
            .publish(&env);
        }
        bump_instance(&env);
    }

    /// Operators can revoke too: a failed AML re-check must not wait for the admin.
    pub fn revoke(env: Env, operator: Address, wallets: Vec<Address>) {
        require_operator(&env, &operator);
        for wallet in wallets.iter() {
            let key = DataKey::Verified(wallet.clone());
            if env.storage().persistent().has(&key) {
                env.storage().persistent().remove(&key);
                Revoked { wallet }.publish(&env);
            }
        }
        bump_instance(&env);
    }

    pub fn is_verified(env: Env, wallet: Address) -> bool {
        match env
            .storage()
            .persistent()
            .get::<_, u32>(&DataKey::Verified(wallet))
        {
            Some(expires_at) => env.ledger().sequence() <= expires_at,
            None => false,
        }
    }

    pub fn expires_at(env: Env, wallet: Address) -> Option<u32> {
        env.storage().persistent().get(&DataKey::Verified(wallet))
    }
}

fn read_admin(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Admin).unwrap()
}

fn require_operator(env: &Env, operator: &Address) {
    operator.require_auth();
    if !env
        .storage()
        .persistent()
        .has(&DataKey::Operator(operator.clone()))
    {
        panic_with_error!(env, Error::NotOperator);
    }
}

fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_THRESHOLD, INSTANCE_BUMP);
}

#[cfg(test)]
mod test;
