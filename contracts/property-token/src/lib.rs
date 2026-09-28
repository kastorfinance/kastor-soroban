//! Fractional property token for Kastor Finance on Soroban.
//!
//! Port of `KastorToken` (ERC-1155 on BSC). On BSC one contract held every
//! property as a separate token id; on Soroban each property gets its own
//! SEP-41 token instance, so wallets, explorers and the Stellar asset tooling
//! see it as a normal fungible token. One unit = one fraction (`decimals = 0`).
//!
//! Rules kept from the Solidity version:
//! - Supply is minted once and never again.
//! - The recipient of any transfer must be KYC-verified.
//! - A restricted mode where only transfer agents move tokens.
//! - A metadata URI (the property's legal file on IPFS) that the admin can lock.
//!
//! Changes:
//! - The sender must be verified too, so revoking a KYC freezes the holder.
//! - Mint also requires a verified recipient.
//! - Agents move tokens through an explicit `force_transfer` instead of
//!   bypassing the owner check inside `transfer`.
#![no_std]

use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype,
    panic_with_error, Address, Env, String,
};

const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_THRESHOLD: u32 = INSTANCE_BUMP - DAY_IN_LEDGERS;
const BALANCE_BUMP: u32 = 120 * DAY_IN_LEDGERS;
const BALANCE_THRESHOLD: u32 = BALANCE_BUMP - 7 * DAY_IN_LEDGERS;

#[contractclient(name = "RegistryClient")]
pub trait Registry {
    fn is_verified(env: Env, wallet: Address) -> bool;
}

#[contracttype]
#[derive(Clone)]
pub struct AllowanceKey {
    pub from: Address,
    pub spender: Address,
}

#[contracttype]
#[derive(Clone)]
pub struct AllowanceValue {
    pub amount: i128,
    pub expiration_ledger: u32,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    Registry,
    Name,
    Symbol,
    MetadataUri,
    MetadataLocked,
    Minted,
    TotalSupply,
    Paused,
    Restricted,
    Agent(Address),
    Balance(Address),
    Allowance(AllowanceKey),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyMinted = 1,
    NegativeAmount = 2,
    InsufficientBalance = 3,
    InsufficientAllowance = 4,
    Paused = 5,
    TransfersRestricted = 6,
    NotVerified = 7,
    NotAgent = 8,
    MetadataLocked = 9,
    InvalidExpiration = 10,
}

#[contractevent(data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transfer {
    #[topic]
    pub from: Address,
    #[topic]
    pub to: Address,
    pub amount: i128,
}

#[contractevent(data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mint {
    #[topic]
    pub to: Address,
    pub amount: i128,
}

#[contractevent(data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Burn {
    #[topic]
    pub from: Address,
    pub amount: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Approve {
    #[topic]
    pub from: Address,
    #[topic]
    pub spender: Address,
    pub amount: i128,
    pub expiration_ledger: u32,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForcedTransfer {
    #[topic]
    pub agent: Address,
    pub from: Address,
    pub to: Address,
    pub amount: i128,
}

#[contract]
pub struct PropertyToken;

#[contractimpl]
impl PropertyToken {
    pub fn __constructor(
        env: Env,
        admin: Address,
        registry: Address,
        name: String,
        symbol: String,
        metadata_uri: String,
    ) {
        let s = env.storage().instance();
        s.set(&DataKey::Admin, &admin);
        s.set(&DataKey::Registry, &registry);
        s.set(&DataKey::Name, &name);
        s.set(&DataKey::Symbol, &symbol);
        s.set(&DataKey::MetadataUri, &metadata_uri);
        s.set(&DataKey::TotalSupply, &0i128);
    }

    // ---------------------------------------------------------------- admin

    /// Mints the whole supply of the property. Can only happen once.
    pub fn mint(env: Env, to: Address, amount: i128) {
        read_admin(&env).require_auth();
        check_positive(&env, amount);
        if env.storage().instance().has(&DataKey::Minted) {
            panic_with_error!(&env, Error::AlreadyMinted);
        }
        require_verified(&env, &to);
        env.storage().instance().set(&DataKey::Minted, &true);
        env.storage().instance().set(&DataKey::TotalSupply, &amount);
        add_balance(&env, &to, amount);
        bump_instance(&env);
        Mint { to, amount }.publish(&env);
    }

    pub fn set_admin(env: Env, new_admin: Address) {
        read_admin(&env).require_auth();
        new_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        bump_instance(&env);
    }

    pub fn set_registry(env: Env, registry: Address) {
        read_admin(&env).require_auth();
        env.storage().instance().set(&DataKey::Registry, &registry);
        bump_instance(&env);
    }

    pub fn set_paused(env: Env, paused: bool) {
        read_admin(&env).require_auth();
        env.storage().instance().set(&DataKey::Paused, &paused);
        bump_instance(&env);
    }

    /// While restricted, holders cannot call `transfer`/`transfer_from`;
    /// only agents move tokens with `force_transfer`.
    pub fn set_restricted(env: Env, restricted: bool) {
        read_admin(&env).require_auth();
        env.storage()
            .instance()
            .set(&DataKey::Restricted, &restricted);
        bump_instance(&env);
    }

    pub fn set_agent(env: Env, agent: Address, enabled: bool) {
        read_admin(&env).require_auth();
        let key = DataKey::Agent(agent);
        if enabled {
            env.storage().instance().set(&key, &true);
        } else {
            env.storage().instance().remove(&key);
        }
        bump_instance(&env);
    }

    pub fn set_metadata_uri(env: Env, uri: String) {
        read_admin(&env).require_auth();
        if env.storage().instance().has(&DataKey::MetadataLocked) {
            panic_with_error!(&env, Error::MetadataLocked);
        }
        env.storage().instance().set(&DataKey::MetadataUri, &uri);
        bump_instance(&env);
    }

    /// Freezes the metadata URI for good (`renounceSetUri` in the BSC version).
    pub fn lock_metadata(env: Env) {
        read_admin(&env).require_auth();
        env.storage()
            .instance()
            .set(&DataKey::MetadataLocked, &true);
        bump_instance(&env);
    }

    /// Agent-initiated transfer (secondary-market settlement, lost-wallet
    /// recovery). Still requires both parties to be verified.
    pub fn force_transfer(env: Env, agent: Address, from: Address, to: Address, amount: i128) {
        agent.require_auth();
        if !env.storage().instance().has(&DataKey::Agent(agent.clone())) {
            panic_with_error!(&env, Error::NotAgent);
        }
        check_positive(&env, amount);
        if is_paused(&env) {
            panic_with_error!(&env, Error::Paused);
        }
        require_verified(&env, &to);
        move_balance(&env, &from, &to, amount);
        ForcedTransfer {
            agent,
            from,
            to,
            amount,
        }
        .publish(&env);
    }

    // --------------------------------------------------------------- SEP-41

    pub fn allowance(env: Env, from: Address, spender: Address) -> i128 {
        read_allowance(&env, &from, &spender).amount
    }

    pub fn approve(
        env: Env,
        from: Address,
        spender: Address,
        amount: i128,
        expiration_ledger: u32,
    ) {
        from.require_auth();
        check_positive_or_zero(&env, amount);
        if amount > 0 && expiration_ledger < env.ledger().sequence() {
            panic_with_error!(&env, Error::InvalidExpiration);
        }
        write_allowance(&env, &from, &spender, amount, expiration_ledger);
        bump_instance(&env);
        Approve {
            from,
            spender,
            amount,
            expiration_ledger,
        }
        .publish(&env);
    }

    pub fn balance(env: Env, id: Address) -> i128 {
        read_balance(&env, &id)
    }

    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        from.require_auth();
        check_transfer(&env, &from, &to, amount);
        move_balance(&env, &from, &to, amount);
        bump_instance(&env);
        Transfer { from, to, amount }.publish(&env);
    }

    pub fn transfer_from(env: Env, spender: Address, from: Address, to: Address, amount: i128) {
        spender.require_auth();
        check_transfer(&env, &from, &to, amount);
        spend_allowance(&env, &from, &spender, amount);
        move_balance(&env, &from, &to, amount);
        bump_instance(&env);
        Transfer { from, to, amount }.publish(&env);
    }

    pub fn burn(env: Env, from: Address, amount: i128) {
        from.require_auth();
        check_positive(&env, amount);
        sub_balance(&env, &from, amount);
        reduce_supply(&env, amount);
        Burn { from, amount }.publish(&env);
    }

    pub fn burn_from(env: Env, spender: Address, from: Address, amount: i128) {
        spender.require_auth();
        check_positive(&env, amount);
        spend_allowance(&env, &from, &spender, amount);
        sub_balance(&env, &from, amount);
        reduce_supply(&env, amount);
        Burn { from, amount }.publish(&env);
    }

    pub fn decimals(_env: Env) -> u32 {
        0
    }

    pub fn name(env: Env) -> String {
        env.storage().instance().get(&DataKey::Name).unwrap()
    }

    pub fn symbol(env: Env) -> String {
        env.storage().instance().get(&DataKey::Symbol).unwrap()
    }

    // ---------------------------------------------------------------- views

    pub fn total_supply(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0)
    }

    pub fn metadata_uri(env: Env) -> String {
        env.storage().instance().get(&DataKey::MetadataUri).unwrap()
    }

    pub fn admin(env: Env) -> Address {
        read_admin(&env)
    }

    pub fn registry(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Registry).unwrap()
    }

    pub fn is_paused(env: Env) -> bool {
        is_paused(&env)
    }

    pub fn is_restricted(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Restricted)
            .unwrap_or(false)
    }
}

// ------------------------------------------------------------------ helpers

fn read_admin(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Admin).unwrap()
}

fn is_paused(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&DataKey::Paused)
        .unwrap_or(false)
}

fn check_positive(env: &Env, amount: i128) {
    if amount <= 0 {
        panic_with_error!(env, Error::NegativeAmount);
    }
}

fn check_positive_or_zero(env: &Env, amount: i128) {
    if amount < 0 {
        panic_with_error!(env, Error::NegativeAmount);
    }
}

fn require_verified(env: &Env, wallet: &Address) {
    let registry: Address = env.storage().instance().get(&DataKey::Registry).unwrap();
    if !RegistryClient::new(env, &registry).is_verified(wallet) {
        panic_with_error!(env, Error::NotVerified);
    }
}

fn check_transfer(env: &Env, from: &Address, to: &Address, amount: i128) {
    check_positive(env, amount);
    if is_paused(env) {
        panic_with_error!(env, Error::Paused);
    }
    if env
        .storage()
        .instance()
        .get(&DataKey::Restricted)
        .unwrap_or(false)
    {
        panic_with_error!(env, Error::TransfersRestricted);
    }
    require_verified(env, from);
    require_verified(env, to);
}

fn read_balance(env: &Env, id: &Address) -> i128 {
    let key = DataKey::Balance(id.clone());
    match env.storage().persistent().get::<_, i128>(&key) {
        Some(balance) => {
            env.storage()
                .persistent()
                .extend_ttl(&key, BALANCE_THRESHOLD, BALANCE_BUMP);
            balance
        }
        None => 0,
    }
}

fn write_balance(env: &Env, id: &Address, amount: i128) {
    let key = DataKey::Balance(id.clone());
    env.storage().persistent().set(&key, &amount);
    env.storage()
        .persistent()
        .extend_ttl(&key, BALANCE_THRESHOLD, BALANCE_BUMP);
}

fn add_balance(env: &Env, id: &Address, amount: i128) {
    let balance = read_balance(env, id);
    write_balance(env, id, balance.checked_add(amount).unwrap());
}

fn sub_balance(env: &Env, id: &Address, amount: i128) {
    let balance = read_balance(env, id);
    if balance < amount {
        panic_with_error!(env, Error::InsufficientBalance);
    }
    write_balance(env, id, balance - amount);
}

fn move_balance(env: &Env, from: &Address, to: &Address, amount: i128) {
    sub_balance(env, from, amount);
    add_balance(env, to, amount);
}

fn reduce_supply(env: &Env, amount: i128) {
    let supply: i128 = env
        .storage()
        .instance()
        .get(&DataKey::TotalSupply)
        .unwrap_or(0);
    env.storage()
        .instance()
        .set(&DataKey::TotalSupply, &(supply - amount));
    bump_instance(env);
}

fn read_allowance(env: &Env, from: &Address, spender: &Address) -> AllowanceValue {
    let key = DataKey::Allowance(AllowanceKey {
        from: from.clone(),
        spender: spender.clone(),
    });
    match env.storage().temporary().get::<_, AllowanceValue>(&key) {
        Some(a) if a.expiration_ledger >= env.ledger().sequence() => a,
        _ => AllowanceValue {
            amount: 0,
            expiration_ledger: 0,
        },
    }
}

fn write_allowance(
    env: &Env,
    from: &Address,
    spender: &Address,
    amount: i128,
    expiration_ledger: u32,
) {
    let key = DataKey::Allowance(AllowanceKey {
        from: from.clone(),
        spender: spender.clone(),
    });
    env.storage().temporary().set(
        &key,
        &AllowanceValue {
            amount,
            expiration_ledger,
        },
    );
    if amount > 0 {
        let live_for = expiration_ledger - env.ledger().sequence();
        env.storage()
            .temporary()
            .extend_ttl(&key, live_for, live_for);
    }
}

fn spend_allowance(env: &Env, from: &Address, spender: &Address, amount: i128) {
    let allowance = read_allowance(env, from, spender);
    if allowance.amount < amount {
        panic_with_error!(env, Error::InsufficientAllowance);
    }
    write_allowance(
        env,
        from,
        spender,
        allowance.amount - amount,
        allowance.expiration_ledger,
    );
}

fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_THRESHOLD, INSTANCE_BUMP);
}

#[cfg(test)]
mod test;
