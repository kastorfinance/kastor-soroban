#![cfg(test)]
extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    vec, Env,
};

fn setup() -> (Env, KycRegistryClient<'static>, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let operator = Address::generate(&env);
    let id = env.register(KycRegistry, (admin.clone(),));
    let client = KycRegistryClient::new(&env, &id);
    client.set_operator(&operator, &true);
    (env, client, admin, operator)
}

#[test]
fn verified_wallet_passes_until_expiry() {
    let (env, client, _, operator) = setup();
    let alice = Address::generate(&env);
    env.ledger().set_sequence_number(100);

    client.verify(&operator, &vec![&env, alice.clone()], &200);
    assert!(client.is_verified(&alice));

    env.ledger().set_sequence_number(200);
    assert!(client.is_verified(&alice), "expiry ledger is inclusive");

    env.ledger().set_sequence_number(201);
    assert!(!client.is_verified(&alice));
}

#[test]
fn unknown_wallet_is_not_verified() {
    let (env, client, _, _) = setup();
    assert!(!client.is_verified(&Address::generate(&env)));
}

#[test]
fn revoke_removes_verification() {
    let (env, client, _, operator) = setup();
    let alice = Address::generate(&env);
    client.verify(&operator, &vec![&env, alice.clone()], &1_000);
    client.revoke(&operator, &vec![&env, alice.clone()]);
    assert!(!client.is_verified(&alice));
    assert_eq!(client.expires_at(&alice), None);
}

#[test]
fn batch_verify() {
    let (env, client, _, operator) = setup();
    let a = Address::generate(&env);
    let b = Address::generate(&env);
    client.verify(&operator, &vec![&env, a.clone(), b.clone()], &1_000);
    assert!(client.is_verified(&a));
    assert!(client.is_verified(&b));
}

#[test]
fn non_operator_cannot_verify() {
    let (env, client, _, _) = setup();
    let intruder = Address::generate(&env);
    let res = client.try_verify(&intruder, &vec![&env, intruder.clone()], &1_000);
    assert_eq!(res, Err(Ok(err(Error::NotOperator))));
}

#[test]
fn disabled_operator_cannot_verify() {
    let (env, client, _, operator) = setup();
    client.set_operator(&operator, &false);
    let res = client.try_verify(&operator, &vec![&env, operator.clone()], &1_000);
    assert_eq!(res, Err(Ok(err(Error::NotOperator))));
}

#[test]
fn expiry_in_the_past_is_rejected() {
    let (env, client, _, operator) = setup();
    env.ledger().set_sequence_number(500);
    let res = client.try_verify(&operator, &vec![&env, operator.clone()], &499);
    assert_eq!(res, Err(Ok(err(Error::ExpiryInPast))));
}

#[test]
#[should_panic]
fn set_operator_requires_admin_auth() {
    let env = Env::default();
    let admin = Address::generate(&env);
    let id = env.register(KycRegistry, (admin,));
    let client = KycRegistryClient::new(&env, &id);
    // No mocked auth: the admin signature is missing.
    client.set_operator(&Address::generate(&env), &true);
}

fn err(e: Error) -> soroban_sdk::Error {
    soroban_sdk::Error::from_contract_error(e as u32)
}
