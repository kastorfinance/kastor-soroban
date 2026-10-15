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

const PRICE: i128 = 55_0000000; // 55 USDC (7 decimals) per unit
const FEE_BPS: u32 = 100; // 1 %
const KYC_UNTIL: u32 = 1_000_000;

fn err(e: Error) -> soroban_sdk::Error {
    soroban_sdk::Error::from_contract_error(e as u32)
}

struct Setup {
    env: Env,
    market: SecondaryMarketClient<'static>,
    token: PropertyTokenClient<'static>,
    usdc: TokenClient<'static>,
    registry: KycRegistryClient<'static>,
    operator: Address,
    treasury: Address,
    seller: Address,
    buyer: Address,
}

fn setup() -> Setup {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_sequence_number(10);

    let admin = Address::generate(&env);
    let operator = Address::generate(&env);
    let treasury = Address::generate(&env);
    let seller = Address::generate(&env);
    let buyer = Address::generate(&env);

    let registry_id = env.register(KycRegistry, (admin.clone(),));
    let registry = KycRegistryClient::new(&env, &registry_id);
    registry.set_operator(&operator, &true);
    registry.verify(
        &operator,
        &vec![&env, seller.clone(), buyer.clone()],
        &KYC_UNTIL,
    );

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
    token.mint(&seller, &100);

    let usdc_sac = env.register_stellar_asset_contract_v2(admin.clone());
    let usdc = TokenClient::new(&env, &usdc_sac.address());
    StellarAssetClient::new(&env, &usdc_sac.address()).mint(&buyer, &(10_000 * 10_000_000));

    let market_id = env.register(
        SecondaryMarket,
        (Config {
            admin: admin.clone(),
            registry: registry_id,
            usdc: usdc_sac.address(),
            treasury: treasury.clone(),
            fee_bps: FEE_BPS,
        },),
    );
    let market = SecondaryMarketClient::new(&env, &market_id);
    market.set_property(&token_id, &true);

    Setup {
        env,
        market,
        token,
        usdc,
        registry,
        operator,
        treasury,
        seller,
        buyer,
    }
}

impl Setup {
    /// Seller approves the market and lists `units` at PRICE until ledger 500.
    fn list(&self, units: i128) -> u64 {
        self.token
            .approve(&self.seller, &self.market.address, &units, &500);
        self.market
            .list(&self.seller, &self.token.address, &units, &PRICE, &500)
    }
}

#[test]
fn full_sale_moves_units_and_pays_seller_and_treasury() {
    let s = setup();
    let id = s.list(10);
    let buyer_usdc = s.usdc.balance(&s.buyer);

    s.market.buy(&s.buyer, &id, &10);

    let cost = PRICE * 10;
    let fee = cost / 100;
    assert_eq!(s.token.balance(&s.buyer), 10);
    assert_eq!(s.token.balance(&s.seller), 90);
    assert_eq!(s.usdc.balance(&s.seller), cost - fee);
    assert_eq!(s.usdc.balance(&s.treasury), fee);
    assert_eq!(s.usdc.balance(&s.buyer), buyer_usdc - cost);
    assert_eq!(
        s.market.listing(&id),
        None,
        "a fully sold listing is removed"
    );
}

#[test]
fn partial_fills_reduce_the_listing() {
    let s = setup();
    let id = s.list(10);
    s.market.buy(&s.buyer, &id, &3);
    assert_eq!(s.market.listing(&id).unwrap().units, 7);
    s.market.buy(&s.buyer, &id, &7);
    assert_eq!(s.market.listing(&id), None);
    assert_eq!(s.token.balance(&s.buyer), 10);
}

#[test]
fn the_market_never_holds_units() {
    let s = setup();
    let id = s.list(10);
    assert_eq!(s.token.balance(&s.market.address), 0);
    assert_eq!(
        s.token.balance(&s.seller),
        100,
        "listing does not move units"
    );
    s.market.buy(&s.buyer, &id, &4);
    assert_eq!(s.token.balance(&s.market.address), 0);
}

#[test]
fn buyer_without_kyc_is_rejected() {
    let s = setup();
    let id = s.list(10);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.market.try_buy(&stranger, &id, &1),
        Err(Ok(err(Error::NotVerified)))
    );
}

#[test]
fn seller_without_kyc_cannot_list_or_sell() {
    let s = setup();
    let id = s.list(10);
    s.registry
        .revoke(&s.operator, &vec![&s.env, s.seller.clone()]);
    assert_eq!(
        s.market.try_buy(&s.buyer, &id, &1),
        Err(Ok(err(Error::NotVerified)))
    );
    assert_eq!(
        s.market
            .try_list(&s.seller, &s.token.address, &1, &PRICE, &500),
        Err(Ok(err(Error::NotVerified)))
    );
}

#[test]
fn expired_listing_cannot_be_bought() {
    let s = setup();
    let id = s.list(10);
    s.env.ledger().set_sequence_number(500);
    s.market.buy(&s.buyer, &id, &1); // last valid ledger
    s.env.ledger().set_sequence_number(501);
    assert_eq!(
        s.market.try_buy(&s.buyer, &id, &1),
        Err(Ok(err(Error::ListingExpired)))
    );
}

#[test]
fn cannot_buy_more_than_listed() {
    let s = setup();
    let id = s.list(5);
    assert_eq!(
        s.market.try_buy(&s.buyer, &id, &6),
        Err(Ok(err(Error::NotEnoughUnits)))
    );
}

#[test]
fn allowance_and_balance_are_checked_at_purchase() {
    let s = setup();
    let id = s.list(10);
    // The seller lowers the approval after listing.
    s.token.approve(&s.seller, &s.market.address, &2, &500);
    assert_eq!(
        s.market.try_buy(&s.buyer, &id, &3),
        Err(Ok(err(Error::AllowanceTooLow)))
    );
    // The seller moves most units away after listing.
    s.token.approve(&s.seller, &s.market.address, &10, &500);
    let other = Address::generate(&s.env);
    s.registry
        .verify(&s.operator, &vec![&s.env, other.clone()], &KYC_UNTIL);
    s.token.transfer(&s.seller, &other, &98);
    assert_eq!(
        s.market.try_buy(&s.buyer, &id, &3),
        Err(Ok(err(Error::SellerBalanceTooLow)))
    );
}

#[test]
fn listing_validates_its_inputs() {
    let s = setup();
    let t = &s.token.address;
    assert_eq!(
        s.market.try_list(&s.seller, t, &0, &PRICE, &500),
        Err(Ok(err(Error::InvalidAmount)))
    );
    assert_eq!(
        s.market.try_list(&s.seller, t, &1, &0, &500),
        Err(Ok(err(Error::InvalidAmount)))
    );
    assert_eq!(
        s.market.try_list(&s.seller, t, &101, &PRICE, &500),
        Err(Ok(err(Error::SellerBalanceTooLow)))
    );
    assert_eq!(
        s.market.try_list(&s.seller, t, &1, &PRICE, &5),
        Err(Ok(err(Error::InvalidExpiration)))
    );
    assert_eq!(
        s.market.try_list(&s.seller, t, &2, &i128::MAX, &500),
        Err(Ok(err(Error::Overflow)))
    );
}

#[test]
fn only_enabled_properties_can_be_traded() {
    let s = setup();
    let id = s.list(10);
    s.market.set_property(&s.token.address, &false);
    assert!(!s.market.is_property_enabled(&s.token.address));
    assert_eq!(
        s.market
            .try_list(&s.seller, &s.token.address, &1, &PRICE, &500),
        Err(Ok(err(Error::PropertyNotEnabled)))
    );
    assert_eq!(
        s.market.try_buy(&s.buyer, &id, &1),
        Err(Ok(err(Error::PropertyNotEnabled)))
    );
}

#[test]
fn seller_cannot_buy_own_listing() {
    let s = setup();
    let id = s.list(10);
    assert_eq!(
        s.market.try_buy(&s.seller, &id, &1),
        Err(Ok(err(Error::SelfTrade)))
    );
}

#[test]
fn seller_and_operator_can_cancel() {
    let s = setup();
    let a = s.list(5);
    assert_eq!(
        s.market.try_cancel(&s.buyer, &a),
        Err(Ok(err(Error::NotSeller)))
    );
    s.market.cancel(&s.seller, &a);
    assert_eq!(s.market.listing(&a), None);
    assert_eq!(
        s.market.try_buy(&s.buyer, &a, &1),
        Err(Ok(err(Error::NoListing)))
    );

    let b = s.list(5);
    s.market.admin_cancel(&b);
    assert_eq!(s.market.listing(&b), None);
    assert_eq!(
        s.market.try_admin_cancel(&b),
        Err(Ok(err(Error::NoListing)))
    );
}

#[test]
fn pause_blocks_listing_and_buying() {
    let s = setup();
    let id = s.list(10);
    s.market.set_paused(&true);
    assert!(s.market.is_paused());
    assert_eq!(
        s.market.try_buy(&s.buyer, &id, &1),
        Err(Ok(err(Error::Paused)))
    );
    assert_eq!(
        s.market
            .try_list(&s.seller, &s.token.address, &1, &PRICE, &500),
        Err(Ok(err(Error::Paused)))
    );
    s.market.cancel(&s.seller, &id); // cancelling still works
    s.market.set_paused(&false);
    assert!(!s.market.is_paused());
}

#[test]
fn fee_is_capped_and_can_be_zero() {
    let s = setup();
    assert_eq!(
        s.market.try_set_fee(&(MAX_FEE_BPS + 1)),
        Err(Ok(err(Error::FeeTooHigh)))
    );
    s.market.set_fee(&0);
    let id = s.list(2);
    s.market.buy(&s.buyer, &id, &2);
    assert_eq!(s.usdc.balance(&s.treasury), 0);
    assert_eq!(s.usdc.balance(&s.seller), PRICE * 2);

    let new_treasury = Address::generate(&s.env);
    s.market.set_treasury(&new_treasury);
    s.market.set_fee(&MAX_FEE_BPS);
    assert_eq!(s.market.config().treasury, new_treasury);
    assert_eq!(s.market.config().fee_bps, MAX_FEE_BPS);
}

#[test]
fn invalid_purchase_amount_is_rejected() {
    let s = setup();
    let id = s.list(2);
    assert_eq!(
        s.market.try_buy(&s.buyer, &id, &0),
        Err(Ok(err(Error::InvalidAmount)))
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #11)")]
fn constructor_rejects_a_fee_above_the_cap() {
    let env = Env::default();
    let a = Address::generate(&env);
    env.register(
        SecondaryMarket,
        (Config {
            admin: a.clone(),
            registry: a.clone(),
            usdc: a.clone(),
            treasury: a,
            fee_bps: MAX_FEE_BPS + 1,
        },),
    );
}

#[test]
fn admin_functions_require_the_admin() {
    let s = setup();
    s.env.set_auths(&[]);
    assert!(s.market.try_set_fee(&5).is_err());
    assert!(s.market.try_set_paused(&true).is_err());
    assert!(s.market.try_set_property(&s.token.address, &false).is_err());
}

#[test]
fn listing_requires_the_seller_signature() {
    let s = setup();
    s.token.approve(&s.seller, &s.market.address, &1, &500);
    s.env.set_auths(&[]);
    assert!(s
        .market
        .try_list(&s.seller, &s.token.address, &1, &PRICE, &500)
        .is_err());
}
