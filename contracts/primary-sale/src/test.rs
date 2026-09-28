#![cfg(test)]
extern crate std;

use super::*;
use kyc_registry::{KycRegistry, KycRegistryClient};
use property_token::{PropertyToken, PropertyTokenClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    vec, Env, String,
};

const PRICE: i128 = 50_0000000; // 50 USDC (7 decimals) per unit
const HOLDING: u32 = 100;
const GRACE: u32 = 1_000;
const SUPPLY: i128 = 1_000;

struct Setup {
    env: Env,
    sale: PrimarySaleClient<'static>,
    token: PropertyTokenClient<'static>,
    usdc: TokenClient<'static>,
    usdc_admin: StellarAssetClient<'static>,
    registry: KycRegistryClient<'static>,
    operator: Address,
    treasury: Address,
}

fn setup() -> Setup {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_sequence_number(10);

    let admin = Address::generate(&env);
    let operator = Address::generate(&env);
    let treasury = Address::generate(&env);

    let registry_id = env.register(KycRegistry, (admin.clone(),));
    let registry = KycRegistryClient::new(&env, &registry_id);
    registry.set_operator(&operator, &true);

    let token_id = env.register(
        PropertyToken,
        (
            admin.clone(),
            registry_id.clone(),
            String::from_str(&env, "Kastor Villa 01"),
            String::from_str(&env, "KV01"),
            String::from_str(&env, "ipfs://bafy-villa-01"),
        ),
    );
    let token = PropertyTokenClient::new(&env, &token_id);

    let usdc_sac = env.register_stellar_asset_contract_v2(admin.clone());
    let usdc = TokenClient::new(&env, &usdc_sac.address());
    let usdc_admin = StellarAssetClient::new(&env, &usdc_sac.address());

    let sale_id = env.register(
        PrimarySale,
        (Config {
            admin: admin.clone(),
            property_token: token_id,
            registry: registry_id,
            usdc: usdc_sac.address(),
            treasury: treasury.clone(),
            unit_price: PRICE,
            holding_ledgers: HOLDING,
            refund_grace_ledgers: GRACE,
        },),
    );
    let sale = PrimarySaleClient::new(&env, &sale_id);

    // The sale contract holds the inventory, so it must pass KYC like any holder.
    registry.verify(&operator, &vec![&env, sale_id.clone()], &u32::MAX);
    token.mint(&sale_id, &SUPPLY);

    Setup {
        env,
        sale,
        token,
        usdc,
        usdc_admin,
        registry,
        operator,
        treasury,
    }
}

impl Setup {
    fn investor(&self, usdc: i128) -> Address {
        let a = Address::generate(&self.env);
        self.registry
            .verify(&self.operator, &vec![&self.env, a.clone()], &u32::MAX);
        self.usdc_admin.mint(&a, &usdc);
        a
    }

    fn advance(&self, ledgers: u32) {
        let seq = self.env.ledger().sequence();
        self.env.ledger().set_sequence_number(seq + ledgers);
    }
}

#[test]
fn buy_escrows_usdc_and_reserves_units() {
    let s = setup();
    let alice = s.investor(1_000 * PRICE);
    s.sale.buy(&alice, &10);

    assert_eq!(s.usdc.balance(&alice), 990 * PRICE);
    assert_eq!(s.usdc.balance(&s.sale.address), 10 * PRICE);
    assert_eq!(s.sale.reserved(), 10);
    assert_eq!(s.sale.available(), SUPPLY - 10);
    assert_eq!(s.token.balance(&alice), 0, "units stay in escrow");
    let p = s.sale.purchase(&alice).unwrap();
    assert_eq!((p.units, p.usdc), (10, 10 * PRICE));
}

#[test]
fn complete_after_holding_delivers_to_buyer_not_operator() {
    let s = setup();
    let alice = s.investor(10 * PRICE);
    s.sale.buy(&alice, &10);

    assert_eq!(
        s.sale.try_complete(&alice),
        Err(Ok(err(Error::HoldingPeriod)))
    );
    s.advance(HOLDING);
    s.sale.complete(&alice);

    assert_eq!(s.token.balance(&alice), 10);
    assert_eq!(s.usdc.balance(&s.treasury), 10 * PRICE);
    assert_eq!(s.usdc.balance(&s.sale.address), 0);
    assert_eq!(s.sale.reserved(), 0);
    assert_eq!(s.sale.purchase(&alice), None);
}

#[test]
fn purchase_cannot_be_settled_twice() {
    let s = setup();
    let alice = s.investor(10 * PRICE);
    s.sale.buy(&alice, &10);
    s.advance(HOLDING);
    s.sale.refund(&alice);
    assert_eq!(s.usdc.balance(&alice), 10 * PRICE);

    assert_eq!(s.sale.try_refund(&alice), Err(Ok(err(Error::NoPurchase))));
    assert_eq!(s.sale.try_complete(&alice), Err(Ok(err(Error::NoPurchase))));
    assert_eq!(s.sale.available(), SUPPLY);
}

#[test]
fn unverified_buyer_rejected() {
    let s = setup();
    let bob = Address::generate(&s.env);
    s.usdc_admin.mint(&bob, &PRICE);
    assert_eq!(s.sale.try_buy(&bob, &1), Err(Ok(err(Error::NotVerified))));
}

#[test]
fn cannot_oversell_reserved_units() {
    let s = setup();
    let alice = s.investor(SUPPLY * PRICE);
    let bob = s.investor(PRICE);
    s.sale.buy(&alice, &SUPPLY);
    assert_eq!(
        s.sale.try_buy(&bob, &1),
        Err(Ok(err(Error::NotEnoughUnits)))
    );
}

#[test]
fn top_up_accumulates_and_restarts_holding() {
    let s = setup();
    let alice = s.investor(20 * PRICE);
    s.sale.buy(&alice, &5);
    s.advance(HOLDING - 1);
    s.sale.buy(&alice, &5);
    s.advance(1);
    assert_eq!(
        s.sale.try_complete(&alice),
        Err(Ok(err(Error::HoldingPeriod)))
    );
    s.advance(HOLDING);
    s.sale.complete(&alice);
    assert_eq!(s.token.balance(&alice), 10);
}

#[test]
fn buyer_self_refund_only_after_grace() {
    let s = setup();
    let alice = s.investor(3 * PRICE);
    s.sale.buy(&alice, &3);
    s.advance(HOLDING + GRACE - 1);
    assert_eq!(
        s.sale.try_claim_refund(&alice),
        Err(Ok(err(Error::RefundNotYetAllowed)))
    );
    s.advance(1);
    s.sale.claim_refund(&alice);
    assert_eq!(s.usdc.balance(&alice), 3 * PRICE);
    assert_eq!(s.sale.reserved(), 0);
}

#[test]
fn completion_fails_if_kyc_revoked_during_holding() {
    let s = setup();
    let alice = s.investor(PRICE);
    s.sale.buy(&alice, &1);
    s.registry.revoke(&s.operator, &vec![&s.env, alice.clone()]);
    s.advance(HOLDING);
    // The token refuses to deliver to a wallet that is no longer verified;
    // the operator has to refund instead.
    assert!(s.sale.try_complete(&alice).is_err());
    s.sale.refund(&alice);
    assert_eq!(s.usdc.balance(&alice), PRICE);
}

#[test]
fn paused_sale_rejects_buys() {
    let s = setup();
    let alice = s.investor(PRICE);
    s.sale.set_paused(&true);
    assert_eq!(s.sale.try_buy(&alice, &1), Err(Ok(err(Error::Paused))));
}

#[test]
fn insufficient_usdc_reverts_whole_buy() {
    let s = setup();
    let alice = s.investor(PRICE - 1);
    assert!(s.sale.try_buy(&alice, &1).is_err());
    assert_eq!(s.sale.reserved(), 0);
    assert_eq!(s.sale.purchase(&alice), None);
}

#[test]
fn withdraw_unsold_respects_reservations() {
    let s = setup();
    let alice = s.investor(10 * PRICE);
    let issuer = s.investor(0);
    s.sale.buy(&alice, &10);
    assert_eq!(
        s.sale.try_withdraw_unsold(&issuer, &(SUPPLY - 9)),
        Err(Ok(err(Error::NotEnoughUnits)))
    );
    s.sale.withdraw_unsold(&issuer, &(SUPPLY - 10));
    assert_eq!(s.token.balance(&issuer), SUPPLY - 10);
    assert_eq!(s.sale.available(), 0);
}

fn err(e: Error) -> soroban_sdk::Error {
    soroban_sdk::Error::from_contract_error(e as u32)
}
