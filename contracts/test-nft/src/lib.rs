use near_sdk::store::IterableMap;
use near_sdk::{env, near, require, AccountId, NearToken, PanicOnDefault, Promise};

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct TestNft {
    price: NearToken,
    owners: IterableMap<String, AccountId>,
}

#[near]
impl TestNft {
    #[init]
    pub fn new(price: NearToken) -> Self {
        Self { price, owners: IterableMap::new(b"o") }
    }

    /// Mint a token. Requires at least `price` attached; refunds the excess to the caller.
    #[payable]
    pub fn nft_mint(&mut self, token_id: String, receiver_id: AccountId) {
        let deposit = env::attached_deposit();
        require!(deposit >= self.price, "attached deposit is below the mint price");
        require!(!self.owners.contains_key(&token_id), "token already minted");
        self.owners.insert(token_id, receiver_id);

        let excess = deposit.saturating_sub(self.price);
        if excess > NearToken::from_yoctonear(0) {
            let _ = Promise::new(env::predecessor_account_id()).transfer(excess);
        }
    }

    pub fn nft_token_owner(&self, token_id: String) -> Option<AccountId> {
        self.owners.get(&token_id).cloned()
    }

    pub fn nft_total_supply(&self) -> u32 {
        self.owners.len()
    }

    pub fn get_price(&self) -> NearToken {
        self.price
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    fn set_deposit(deposit: NearToken) {
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id("vault.testnet".parse().unwrap())
            .attached_deposit(deposit)
            .build());
    }

    fn contract() -> TestNft {
        set_deposit(NearToken::from_yoctonear(0));
        TestNft::new(NearToken::from_millinear(500))
    }

    #[test]
    fn mints_at_price() {
        let mut c = contract();
        set_deposit(NearToken::from_millinear(500));
        c.nft_mint("t1".to_string(), "alice.testnet".parse().unwrap());
        assert_eq!(c.nft_total_supply(), 1);
        assert_eq!(c.nft_token_owner("t1".to_string()), Some("alice.testnet".parse().unwrap()));
    }

    #[test]
    #[should_panic(expected = "below the mint price")]
    fn rejects_below_price() {
        let mut c = contract();
        set_deposit(NearToken::from_millinear(100));
        c.nft_mint("t1".to_string(), "alice.testnet".parse().unwrap());
    }

    #[test]
    #[should_panic(expected = "already minted")]
    fn rejects_duplicate_token() {
        let mut c = contract();
        set_deposit(NearToken::from_millinear(500));
        c.nft_mint("t1".to_string(), "alice.testnet".parse().unwrap());
        c.nft_mint("t1".to_string(), "bob.testnet".parse().unwrap());
    }
}