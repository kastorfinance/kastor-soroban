#![cfg(test)]
extern crate std;

use super::*;
use kyc_registry::{KycRegistry, KycRegistryClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    vec, Env, String,
};

struct Setup {
    env: Env,
    token: PropertyTokenClient<'static>,
    registry: KycRegistryClient<'static>,
    operator: Address,
    issuer: Address,
}

fn setup() -> Setup {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let operator = Address::generate(&env);
    let issuer = Address::generate(&env);

    let registry_id = env.register(KycRegistry, (admin.clone(),));
    let registry = KycRegistryClient::new(&env, &registry_id);
    registry.set_operator(&operator, &true);
    registry.verify(&operator, &vec![&env, issuer.clone()], &1_000_000);

    let token_id = env.register(
        PropertyToken,
        (
            admin,
            registry_id,
            String::from_str(&env, "Kastor Villa 01"),
            String::from_str(&env, "KV01"),
            String::from_str(&env, "ipfs://bafy-villa-01"),
        ),
    );
    let token = PropertyTokenClient::new(&env, &token_id);
    token.mint(&issuer, &1_000);
    Setup {
        env,
        token,
        registry,
        operator,
        issuer,
    }
}

impl Setup {
    fn verified(&self) -> Address {
        let a = Address::generate(&self.env);
        self.registry
            .verify(&self.operator, &vec![&self.env, a.clone()], &1_000_000);
        a
    }
}

#[test]
fn metadata() {
    let s = setup();
    assert_eq!(s.token.decimals(), 0);
    assert_eq!(s.token.symbol(), String::from_str(&s.env, "KV01"));
    assert_eq!(s.token.total_supply(), 1_000);
    assert_eq!(s.token.balance(&s.issuer), 1_000);
}

#[test]
fn mint_only_once() {
    let s = setup();
    let res = s.token.try_mint(&s.issuer, &1);
    assert_eq!(res, Err(Ok(err(Error::AlreadyMinted))));
}

#[test]
fn transfer_to_verified_wallet() {
    let s = setup();
    let alice = s.verified();
    s.token.transfer(&s.issuer, &alice, &10);
    assert_eq!(s.token.balance(&alice), 10);
    assert_eq!(s.token.balance(&s.issuer), 990);
}

#[test]
fn transfer_to_unverified_wallet_fails() {
    let s = setup();
    let stranger = Address::generate(&s.env);
    let res = s.token.try_transfer(&s.issuer, &stranger, &10);
    assert_eq!(res, Err(Ok(err(Error::NotVerified))));
    assert_eq!(s.token.balance(&s.issuer), 1_000);
}

#[test]
fn revoked_holder_is_frozen() {
    let s = setup();
    let alice = s.verified();
    let bob = s.verified();
    s.token.transfer(&s.issuer, &alice, &10);
    s.registry.revoke(&s.operator, &vec![&s.env, alice.clone()]);
    let res = s.token.try_transfer(&alice, &bob, &5);
    assert_eq!(res, Err(Ok(err(Error::NotVerified))));
}

#[test]
fn expired_kyc_blocks_receiving() {
    let s = setup();
    let alice = Address::generate(&s.env);
    s.registry
        .verify(&s.operator, &vec![&s.env, alice.clone()], &50);
    s.env.ledger().set_sequence_number(51);
    let res = s.token.try_transfer(&s.issuer, &alice, &1);
    assert_eq!(res, Err(Ok(err(Error::NotVerified))));
}

#[test]
fn paused_blocks_transfers() {
    let s = setup();
    let alice = s.verified();
    s.token.set_paused(&true);
    assert_eq!(
        s.token.try_transfer(&s.issuer, &alice, &1),
        Err(Ok(err(Error::Paused)))
    );
    s.token.set_paused(&false);
    s.token.transfer(&s.issuer, &alice, &1);
}

#[test]
fn restricted_mode_only_agents() {
    let s = setup();
    let alice = s.verified();
    let agent = Address::generate(&s.env);
    s.token.set_restricted(&true);
    assert_eq!(
        s.token.try_transfer(&s.issuer, &alice, &1),
        Err(Ok(err(Error::TransfersRestricted)))
    );
    assert_eq!(
        s.token.try_force_transfer(&agent, &s.issuer, &alice, &1),
        Err(Ok(err(Error::NotAgent)))
    );
    s.token.set_agent(&agent, &true);
    s.token.force_transfer(&agent, &s.issuer, &alice, &1);
    assert_eq!(s.token.balance(&alice), 1);
}

#[test]
fn force_transfer_still_needs_verified_recipient() {
    let s = setup();
    let agent = Address::generate(&s.env);
    s.token.set_agent(&agent, &true);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.token.try_force_transfer(&agent, &s.issuer, &stranger, &1),
        Err(Ok(err(Error::NotVerified)))
    );
}

#[test]
fn insufficient_balance() {
    let s = setup();
    let alice = s.verified();
    assert_eq!(
        s.token.try_transfer(&alice, &s.issuer, &1),
        Err(Ok(err(Error::InsufficientBalance)))
    );
}

#[test]
fn zero_or_negative_amount_rejected() {
    let s = setup();
    let alice = s.verified();
    assert_eq!(
        s.token.try_transfer(&s.issuer, &alice, &0),
        Err(Ok(err(Error::NegativeAmount)))
    );
    assert_eq!(
        s.token.try_transfer(&s.issuer, &alice, &-5),
        Err(Ok(err(Error::NegativeAmount)))
    );
}

#[test]
fn allowance_flow_and_expiry() {
    let s = setup();
    let alice = s.verified();
    let spender = Address::generate(&s.env);
    s.token.approve(&s.issuer, &spender, &100, &200);
    assert_eq!(s.token.allowance(&s.issuer, &spender), 100);

    s.token.transfer_from(&spender, &s.issuer, &alice, &60);
    assert_eq!(s.token.allowance(&s.issuer, &spender), 40);
    assert_eq!(
        s.token.try_transfer_from(&spender, &s.issuer, &alice, &41),
        Err(Ok(err(Error::InsufficientAllowance)))
    );

    s.env.ledger().set_sequence_number(201);
    assert_eq!(s.token.allowance(&s.issuer, &spender), 0);
}

#[test]
fn burn_reduces_supply() {
    let s = setup();
    s.token.burn(&s.issuer, &100);
    assert_eq!(s.token.total_supply(), 900);
    assert_eq!(s.token.balance(&s.issuer), 900);
}

#[test]
fn metadata_lock() {
    let s = setup();
    let uri = String::from_str(&s.env, "ipfs://bafy-villa-01-v2");
    s.token.set_metadata_uri(&uri);
    assert_eq!(s.token.metadata_uri(), uri);
    s.token.lock_metadata();
    assert_eq!(
        s.token.try_set_metadata_uri(&uri),
        Err(Ok(err(Error::MetadataLocked)))
    );
}

fn err(e: Error) -> soroban_sdk::Error {
    soroban_sdk::Error::from_contract_error(e as u32)
}
