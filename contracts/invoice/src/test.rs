#![cfg(test)]

use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, TestRunner};
use soroban_sdk::{
    contract, contractimpl, contracttype,
    testutils::{Address as _, Events as _, Ledger},
    token, Address, BytesN, Env, IntoVal, Symbol, TryFromVal,
};

use crate::{InvoiceContract, InvoiceContractClient, InvoiceStatus};

#[contract]
pub struct MockRegistry;

#[contractimpl]
impl MockRegistry {
    pub fn is_verified(env: Env, address: Address) -> bool {
        env.storage()
            .persistent()
            .get::<_, bool>(&DataKey(address))
            .unwrap_or(false)
    }

    pub fn register(env: Env, address: Address) {
        env.storage()
            .persistent()
            .set(&DataKey(address.clone()), &true);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey(address), 100, 2_000_000);
    }
}

#[contracttype]
pub struct DataKey(Address);

// --------------- Mock Token ---------------

#[contract]
pub struct MockToken;

#[contractimpl]
impl MockToken {
    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        let from_key = TKey(from.clone());
        let to_key = TKey(to.clone());
        let from_bal: i128 = env.storage().persistent().get(&from_key).unwrap_or(0);
        let to_bal: i128 = env.storage().persistent().get(&to_key).unwrap_or(0);
        env.storage()
            .persistent()
            .set(&from_key, &(from_bal - amount));
        env.storage().persistent().set(&to_key, &(to_bal + amount));
    }

    pub fn balance(env: Env, addr: Address) -> i128 {
        env.storage().persistent().get(&TKey(addr)).unwrap_or(0)
    }

    pub fn mint(env: Env, to: Address, amount: i128) {
        let key = TKey(to.clone());
        let bal: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(bal + amount));
    }
}

#[contracttype]
pub struct TKey(Address);

#[contract]
pub struct MockPool;

#[contractimpl]
impl MockPool {
    pub fn handle_default(_env: Env, _invoice_id: BytesN<32>) -> bool {
        true
    }

    pub fn receive_repayment(_env: Env, _invoice_id: BytesN<32>, _amount: u128) -> bool {
        true
    }

    pub fn get_usdc_asset(env: Env) -> Address {
        let key = Symbol::new(&env, "asset");
        env.storage().instance().get(&key).unwrap()
    }

    pub fn receive_repayment_with_refund(
        env: Env,
        _invoice_id: BytesN<32>,
        _amount: u128,
        refund: u128,
        _buyer: Address,
    ) -> bool {
        let key = Symbol::new(&env, "last_refund");
        env.storage().instance().set(&key, &refund);
        true
    }

    pub fn get_last_refund(env: Env) -> u128 {
        let key = Symbol::new(&env, "last_refund");
        env.storage().instance().get(&key).unwrap_or(0)
    }
}

type Setup = (
    Env,
    InvoiceContractClient<'static>,
    Address,
    Address,
    MockRegistryClient<'static>,
    Address,
);

fn setup() -> Setup {
    let env = Env::default();
    env.mock_all_auths();

    let registry_id = env.register_contract(None, MockRegistry);
    let registry_client = MockRegistryClient::new(&env, &registry_id);

    let issuer = Address::generate(&env);
    let buyer = Address::generate(&env);
    registry_client.register(&issuer);
    registry_client.register(&buyer);

    let contract_id = env.register_contract(None, InvoiceContract);
    let client = InvoiceContractClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    client.initialize(&admin, &registry_id);

    let usdc_asset = env.register_contract(None, MockToken);
    client.add_supported_asset(&usdc_asset);

    (env, client, issuer, buyer, registry_client, usdc_asset)
}

#[allow(dead_code)]
type SetupWithAdmin = (
    Env,
    InvoiceContractClient<'static>,
    Address,
    Address,
    MockRegistryClient<'static>,
    Address,
    Address,
);

#[allow(dead_code)]
fn setup_with_admin() -> SetupWithAdmin {
    let env = Env::default();
    env.mock_all_auths();

    let registry_id = env.register_contract(None, MockRegistry);
    let registry_client = MockRegistryClient::new(&env, &registry_id);

    let issuer = Address::generate(&env);
    let buyer = Address::generate(&env);
    registry_client.register(&issuer);
    registry_client.register(&buyer);

    let contract_id = env.register_contract(None, InvoiceContract);
    let client = InvoiceContractClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    client.initialize(&admin, &registry_id);

    let usdc_asset = env.register_contract(None, MockToken);
    client.add_supported_asset(&usdc_asset);

    (
        env,
        client,
        issuer,
        buyer,
        registry_client,
        usdc_asset,
        admin,
    )
}

fn mock_pool_with_asset(env: &Env, asset: &Address) -> Address {
    let pool_id = env.register_contract(None, MockPool);
    let _pool_client = MockPoolClient::new(env, &pool_id);
    env.as_contract(&pool_id, || {
        let key = Symbol::new(env, "asset");
        env.storage().instance().set(&key, asset);
    });
    pool_id
}

// --------------- Mock Escrow ---------------

/// Minimal mock escrow that records the pool address and implements
/// `release_to_pool` so `invoice::repay` / `invoice::repay_early` can call
/// it without needing the full real escrow contract.
#[contract]
pub struct MockEscrow;

#[contractimpl]
impl MockEscrow {
    /// Stores the pool address so `release_to_pool` knows where to forward.
    pub fn set_pool(env: Env, pool: Address) {
        env.storage().instance().set(&Symbol::new(&env, "pool"), &pool);
    }

    /// Minimal stub: in the mock token world, `release_to_pool` just transfers
    /// the repayment amount from escrow back to pool (mirrors real escrow logic
    /// but without the lock-record validation that the real escrow enforces).
    pub fn release_to_pool(env: Env, _invoice_id: BytesN<32>, amount: u128) -> bool {
        // Require pool auth (mirrors the real escrow's require_pool_auth).
        let pool: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "pool"))
            .unwrap();
        pool.require_auth();

        // Forward the held token from escrow -> pool so token balances in tests
        // reflect the repayment flow. We ask the pool for its USDC asset via a
        // helper `get_usdc_asset()` that MockPool exposes in tests, then use the
        // token client to transfer `amount` from this contract (escrow) to pool.
        let asset: Address = env.invoke_contract(&pool, &Symbol::new(&env, "get_usdc_asset"), Vec::new(&env));

        // Current contract address acts as escrow address
        let escrow_addr = env.current_contract_address();

        // Transfer amount from escrow -> pool using the token client.
        let token_client = token::Client::new(&env, &asset);
        token_client.transfer(&escrow_addr, &pool, &(amount as i128));

        true
    }
}

/// Registers a MockEscrow wired to `pool_id` and returns its address.
fn mock_escrow_for_pool(env: &Env, pool_id: &Address) -> Address {
    let escrow_id = env.register_contract(None, MockEscrow);
    MockEscrowClient::new(env, &escrow_id).set_pool(pool_id);
    escrow_id
}


#[test]
fn test_create_invoice_with_verified_parties() {
    let (env, client, issuer, buyer, _, usdc) = setup();
    let face_value: u128 = 1_000_000_000;
    let due_date = env.ledger().timestamp() + 86400;

    let invoice_id = client.create(&issuer, &buyer, &face_value, &due_date, &usdc);
    let invoice = client.get(&invoice_id);

    assert_eq!(invoice.issuer, issuer);
    assert_eq!(invoice.buyer, buyer);
    assert_eq!(invoice.face_value, face_value);
    assert_eq!(invoice.due_date, due_date);
    assert_eq!(invoice.status, InvoiceStatus::Created);
    assert_eq!(invoice.funding_asset, usdc);
    assert_eq!(invoice.funding_pool, None);
    assert!(!invoice.issuer_confirmed);
    assert!(!invoice.buyer_confirmed);
}
