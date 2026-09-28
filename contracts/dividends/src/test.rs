#![cfg(test)]
extern crate std;

use super::*;
use kyc_registry::{KycRegistry, KycRegistryClient};
use property_token::{PropertyToken, PropertyTokenClient as TokenCl};
use soroban_sdk::{
    testutils::Address as _,
    token::{StellarAssetClient, TokenClient},
    vec, Env, String,
};

struct Setup {
    env: Env,
    div: DividendsClient<'static>,
    token: TokenCl<'static>,
    usdc: TokenClient<'static>,
    admin: Address,
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
    registry.verify(&operator, &vec![&env, issuer.clone()], &u32::MAX);

    let token_id = env.register(
        PropertyToken,
        (
            admin.clone(),
            registry_id,
            String::from_str(&env, "Kastor Villa 01"),
            String::from_str(&env, "KV01"),
            String::from_str(&env, "ipfs://bafy-villa-01"),
        ),
    );
    let token = TokenCl::new(&env, &token_id);
    token.mint(&issuer, &1_000);

    let usdc_sac = env.register_stellar_asset_contract_v2(admin.clone());
    let usdc = TokenClient::new(&env, &usdc_sac.address());
    StellarAssetClient::new(&env, &usdc_sac.address()).mint(&admin, &1_000_000);

    let div_id = env.register(Dividends, (admin.clone(), usdc_sac.address(), token_id));
    let div = DividendsClient::new(&env, &div_id);
    Setup {
        env,
        div,
        token,
        usdc,
        admin,
        registry,
        operator,
        issuer,
    }
}

impl Setup {
    fn holder(&self, units: i128) -> Address {
        let a = Address::generate(&self.env);
        self.registry
            .verify(&self.operator, &vec![&self.env, a.clone()], &u32::MAX);
        self.token.transfer(&self.issuer, &a, &units);
        a
    }
}

#[test]
fn allocate_funds_and_credits() {
    let s = setup();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let total = s
        .div
        .allocate(&vec![&s.env, a.clone(), b.clone()], &vec![&s.env, 300, 700]);
    assert_eq!(total, 1_000);
    assert_eq!(s.usdc.balance(&s.div.address), 1_000);
    assert_eq!(s.usdc.balance(&s.admin), 999_000);
    assert_eq!(s.div.claimable(&a), 300);
    assert_eq!(s.div.claimable(&b), 700);
}

#[test]
fn claim_pays_once() {
    let s = setup();
    let a = Address::generate(&s.env);
    s.div.allocate(&vec![&s.env, a.clone()], &vec![&s.env, 500]);
    assert_eq!(s.div.claim(&a), 500);
    assert_eq!(s.usdc.balance(&a), 500);
    // The BSC contract let this second call succeed.
    assert_eq!(s.div.try_claim(&a), Err(Ok(err(Error::NothingToClaim))));
    assert_eq!(s.usdc.balance(&a), 500);
}

#[test]
fn allocations_accumulate_across_rounds() {
    let s = setup();
    let a = Address::generate(&s.env);
    s.div.allocate(&vec![&s.env, a.clone()], &vec![&s.env, 100]);
    s.div.allocate(&vec![&s.env, a.clone()], &vec![&s.env, 50]);
    assert_eq!(s.div.claim(&a), 150);
}

#[test]
fn mismatched_input_rejected() {
    let s = setup();
    let a = Address::generate(&s.env);
    assert_eq!(
        s.div
            .try_allocate(&vec![&s.env, a.clone()], &vec![&s.env, 1, 2]),
        Err(Ok(err(Error::InvalidInput)))
    );
    assert_eq!(
        s.div.try_allocate(&vec![&s.env, a], &vec![&s.env, 0]),
        Err(Ok(err(Error::InvalidInput)))
    );
}

#[test]
fn pro_rata_by_token_balance() {
    let s = setup();
    let a = s.holder(250); // 25 %
    let b = s.holder(100); // 10 %
    let total = s
        .div
        .allocate_pro_rata(&vec![&s.env, a.clone(), b.clone()], &10_000);
    assert_eq!(s.div.claimable(&a), 2_500);
    assert_eq!(s.div.claimable(&b), 1_000);
    assert_eq!(total, 3_500);
    // Only the allocated sum leaves the admin wallet.
    assert_eq!(s.usdc.balance(&s.div.address), 3_500);
}

#[test]
fn pro_rata_rounds_down_and_keeps_dust() {
    let s = setup();
    let a = s.holder(333);
    let total = s.div.allocate_pro_rata(&vec![&s.env, a.clone()], &100);
    assert_eq!(total, 33); // 100 * 333 / 1000 = 33.3
    assert_eq!(s.usdc.balance(&s.admin), 1_000_000 - 33);
}

#[test]
fn pro_rata_rejects_duplicates() {
    let s = setup();
    let a = s.holder(100);
    assert_eq!(
        s.div
            .try_allocate_pro_rata(&vec![&s.env, a.clone(), a.clone()], &1_000),
        Err(Ok(err(Error::DuplicateHolder)))
    );
}

#[test]
fn paused_blocks_claims() {
    let s = setup();
    let a = Address::generate(&s.env);
    s.div.allocate(&vec![&s.env, a.clone()], &vec![&s.env, 10]);
    s.div.set_paused(&true);
    assert_eq!(s.div.try_claim(&a), Err(Ok(err(Error::Paused))));
    s.div.set_paused(&false);
    s.div.claim(&a);
}

fn err(e: Error) -> soroban_sdk::Error {
    soroban_sdk::Error::from_contract_error(e as u32)
}
