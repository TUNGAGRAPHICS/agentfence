//! AgentFence vault.
//!
//! An owner funds a budget for one agent account. The agent holds only a function-call key
//! limited to `request_spend` on this contract, and every spend is checked against the owner's
//! rules before any money moves. Rejections do not panic: they are stored and logged so they can
//! be shown in a dashboard and verified in the explorer.
//!
//! Safety properties (each has unit tests):
//!   1. A spend never exceeds `max_per_tx`.
//!   2. Total spend never exceeds the funded budget (the ledger is reduced BEFORE the call).
//!   3. Money only goes to allowlisted (receiver, method) pairs.
//!   4. `request_id` must be `last_request_id + 1`, so an executed request can never be replayed.
//!   5. A failed target call refunds the ledger in the callback.
//!   6. Policies are keyed by (owner, agent), so nobody can create a policy for an agent they do
//!      not control on someone else's behalf, and only the owner can change or withdraw.
//!   7. Pause stops all new approvals.
//!   8. Withdraw can only take ledger funds, never funds reserved for an in-flight spend.
//!
//! Not audited. Prototype only.

use near_sdk::json_types::U64;
use near_sdk::serde_json::{json, Value};
use near_sdk::store::{IterableMap, Vector};
use near_sdk::{
    assert_one_yocto, env, is_promise_success, near, require, AccountId, BorshStorageKey, Gas,
    NearToken, PanicOnDefault, Promise, PromiseOrValue,
};

const GAS_TARGET_CALL: Gas = Gas::from_tgas(40);
const GAS_CALLBACK: Gas = Gas::from_tgas(10);
/// Target call + callback + overhead for this method itself.
const MIN_PREPAID_GAS: Gas = Gas::from_tgas(70);

/// The request log is a ring buffer so a spamming agent key cannot grow storage without bound
/// (storage staking is paid from the vault account's balance).
const LOG_CAPACITY: u64 = 200;
const MAX_ALLOWED_TARGETS: usize = 20;
const MAX_METHOD_LEN: usize = 128;
const MAX_ARGS_LEN: usize = 2048;
const MAX_VIEW_LIMIT: u64 = 100;

#[near(serializers = [borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    Policies,
    Log,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq)]
pub struct AllowedTarget {
    pub receiver: AccountId,
    pub method: String,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    pub owner: AccountId,
    pub agent: AccountId,
    /// Budget the agent can still spend. Reserved (in-flight) amounts are already removed.
    pub balance: NearToken,
    pub max_per_tx: NearToken,
    pub allowed: Vec<AllowedTarget>,
    pub paused: bool,
    pub total_deposited: NearToken,
    pub total_spent: NearToken,
    /// The next request must use `last_request_id + 1`.
    pub last_request_id: U64,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq)]
pub enum RejectReason {
    /// The request id was already used (replay or stale id).
    Duplicate,
    Paused,
    TargetNotAllowed,
    ExceedsTxLimit,
    InsufficientBudget,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq)]
pub enum RequestStatus {
    /// Approved, the target call is in flight and the amount is reserved.
    Pending,
    Executed,
    /// The target call failed and the amount was returned to the ledger.
    FailedRefunded,
    Rejected(RejectReason),
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq)]
pub struct RequestRecord {
    /// Global, ever-increasing record number.
    pub id: U64,
    pub owner: AccountId,
    pub agent: AccountId,
    pub request_id: U64,
    pub receiver: AccountId,
    pub method: String,
    pub amount: NearToken,
    pub status: RequestStatus,
    pub timestamp_ns: U64,
}

#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq)]
pub struct SpendResult {
    pub id: U64,
    pub status: RequestStatus,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Vault {
    policies: IterableMap<(AccountId, AccountId), Policy>,
    log: Vector<RequestRecord>,
    next_record_id: u64,
}

#[near]
impl Vault {
    #[init]
    pub fn new() -> Self {
        Self {
            policies: IterableMap::new(StorageKey::Policies),
            log: Vector::new(StorageKey::Log),
            next_record_id: 0,
        }
    }

    // ------------------------------------------------------------------------------------
    // Owner methods. The caller is the owner; the policy key is (caller, agent).
    // Anything that loosens a rule or moves money needs exactly 1 yoctoNEAR attached. A
    // function-call access key cannot attach a deposit, so such a key (for example the one a
    // dApp login creates) can never loosen rules or withdraw. Pause, remove and top up are
    // safe directions and need no yocto.
    // ------------------------------------------------------------------------------------

    /// Creates a policy for `agent` and funds it with the attached deposit.
    #[payable]
    pub fn create_policy(
        &mut self,
        agent: AccountId,
        max_per_tx: NearToken,
        allowed: Vec<AllowedTarget>,
    ) {
        let owner = env::predecessor_account_id();
        require!(agent != owner, "the agent must be a different account than the owner");
        require!(agent != env::current_account_id(), "the agent cannot be the vault itself");
        require!(allowed.len() <= MAX_ALLOWED_TARGETS, "too many allowed targets");
        let key = (owner.clone(), agent.clone());
        require!(self.policies.get(&key).is_none(), "this owner already has a policy for the agent");

        let mut list: Vec<AllowedTarget> = Vec::new();
        for target in allowed {
            Self::validate_target(&target.receiver, &target.method);
            if !list.contains(&target) {
                list.push(target);
            }
        }

        let deposit = env::attached_deposit();
        let policy = Policy {
            owner: owner.clone(),
            agent: agent.clone(),
            balance: deposit,
            max_per_tx,
            allowed: list,
            paused: false,
            total_deposited: deposit,
            total_spent: NearToken::from_yoctonear(0),
            last_request_id: U64(0),
        };
        self.policies.insert(key, policy);
        emit(
            "policy_created",
            json!({
                "owner": owner,
                "agent": agent,
                "budget_yocto": deposit.as_yoctonear().to_string(),
                "max_per_tx_yocto": max_per_tx.as_yoctonear().to_string(),
            }),
        );
    }

    /// Adds the attached deposit to the agent's budget.
    #[payable]
    pub fn top_up(&mut self, agent: AccountId) {
        let deposit = env::attached_deposit();
        require!(deposit.as_yoctonear() > 0, "attach a deposit to top up");
        let owner = env::predecessor_account_id();
        let policy = self.policy_mut(&agent);
        policy.balance = policy.balance.saturating_add(deposit);
        policy.total_deposited = policy.total_deposited.saturating_add(deposit);
        emit(
            "topped_up",
            json!({ "owner": owner, "agent": agent, "amount_yocto": deposit.as_yoctonear().to_string() }),
        );
    }

    #[payable]
    pub fn set_max_per_tx(&mut self, agent: AccountId, max_per_tx: NearToken) {
        assert_one_yocto();
        let owner = env::predecessor_account_id();
        self.policy_mut(&agent).max_per_tx = max_per_tx;
        emit(
            "max_per_tx_changed",
            json!({ "owner": owner, "agent": agent, "max_per_tx_yocto": max_per_tx.as_yoctonear().to_string() }),
        );
    }

    #[payable]
    pub fn add_allowed_target(&mut self, agent: AccountId, receiver: AccountId, method: String) {
        assert_one_yocto();
        Self::validate_target(&receiver, &method);
        let owner = env::predecessor_account_id();
        let policy = self.policy_mut(&agent);
        let target = AllowedTarget { receiver: receiver.clone(), method: method.clone() };
        require!(!policy.allowed.contains(&target), "target already allowed");
        require!(policy.allowed.len() < MAX_ALLOWED_TARGETS, "too many allowed targets");
        policy.allowed.push(target);
        emit(
            "target_added",
            json!({ "owner": owner, "agent": agent, "receiver": receiver, "method": method }),
        );
    }

    pub fn remove_allowed_target(&mut self, agent: AccountId, receiver: AccountId, method: String) {
        let owner = env::predecessor_account_id();
        let policy = self.policy_mut(&agent);
        let target = AllowedTarget { receiver: receiver.clone(), method: method.clone() };
        let before = policy.allowed.len();
        policy.allowed.retain(|t| t != &target);
        require!(policy.allowed.len() < before, "target was not in the allowlist");
        emit(
            "target_removed",
            json!({ "owner": owner, "agent": agent, "receiver": receiver, "method": method }),
        );
    }

    /// Emergency stop. Safe direction, so no yocto needed.
    pub fn pause(&mut self, agent: AccountId) {
        let owner = env::predecessor_account_id();
        self.policy_mut(&agent).paused = true;
        emit("paused", json!({ "owner": owner, "agent": agent }));
    }

    #[payable]
    pub fn resume(&mut self, agent: AccountId) {
        assert_one_yocto();
        let owner = env::predecessor_account_id();
        self.policy_mut(&agent).paused = false;
        emit("resumed", json!({ "owner": owner, "agent": agent }));
    }

    /// Returns unused budget to the owner. Reserved (in-flight) funds are not in the ledger, so
    /// they cannot be withdrawn. If the transfer fails the ledger is restored in the callback.
    #[payable]
    pub fn withdraw(&mut self, agent: AccountId, amount: NearToken) -> Promise {
        assert_one_yocto();
        require!(amount.as_yoctonear() > 0, "amount must be positive");
        let owner = env::predecessor_account_id();
        let policy = self.policy_mut(&agent);
        require!(amount <= policy.balance, "amount exceeds the available budget");
        policy.balance = policy.balance.saturating_sub(amount);
        emit(
            "withdraw_requested",
            json!({ "owner": owner, "agent": agent, "amount_yocto": amount.as_yoctonear().to_string() }),
        );
        Promise::new(owner.clone()).transfer(amount).then(
            Self::ext(env::current_account_id())
                .with_static_gas(GAS_CALLBACK)
                .on_withdraw_result(owner, agent, amount),
        )
    }

    // ------------------------------------------------------------------------------------
    // Agent method. The agent account's function-call key is limited to this method.
    // ------------------------------------------------------------------------------------

    /// Asks the vault to call `receiver.method(args)` with `amount` attached.
    ///
    /// The caller is the agent. A request that breaks the rules is recorded as rejected and
    /// returns normally. Only a caller with no policy, a skipped request id, oversized input or
    /// too little gas panics.
    pub fn request_spend(
        &mut self,
        owner: AccountId,
        request_id: U64,
        receiver: AccountId,
        method: String,
        args: String,
        amount: NearToken,
    ) -> PromiseOrValue<SpendResult> {
        let agent = env::predecessor_account_id();
        let key = (owner.clone(), agent.clone());
        require!(method.len() <= MAX_METHOD_LEN, "method name too long");
        require!(args.len() <= MAX_ARGS_LEN, "args too long");
        require!(env::prepaid_gas() >= MIN_PREPAID_GAS, "attach at least 70 TGas");
        let rid = request_id.0;

        let reason = {
            let policy = self
                .policies
                .get(&key)
                .unwrap_or_else(|| env::panic_str("caller is not a registered agent for this owner"));
            let last = policy.last_request_id.0;
            require!(
                rid <= last.saturating_add(1),
                "request_id must be last_request_id + 1"
            );
            if rid <= last {
                Some(RejectReason::Duplicate)
            } else if policy.paused {
                Some(RejectReason::Paused)
            } else if !policy
                .allowed
                .iter()
                .any(|t| t.receiver == receiver && t.method == method)
            {
                Some(RejectReason::TargetNotAllowed)
            } else if amount > policy.max_per_tx {
                Some(RejectReason::ExceedsTxLimit)
            } else if amount > policy.balance {
                Some(RejectReason::InsufficientBudget)
            } else {
                None
            }
        };

        let record = RequestRecord {
            id: U64(0),
            owner: owner.clone(),
            agent: agent.clone(),
            request_id,
            receiver: receiver.clone(),
            method: method.clone(),
            amount,
            status: RequestStatus::Pending,
            timestamp_ns: U64(env::block_timestamp()),
        };

        match reason {
            Some(reason) => {
                if reason != RejectReason::Duplicate {
                    // The id is consumed even though the request is rejected.
                    if let Some(policy) = self.policies.get_mut(&key) {
                        policy.last_request_id = request_id;
                    }
                }
                let status = RequestStatus::Rejected(reason.clone());
                let id = self.push_log(RequestRecord { status: status.clone(), ..record });
                emit(
                    "request_rejected",
                    json!({
                        "owner": owner, "agent": agent, "record_id": id.to_string(),
                        "request_id": rid.to_string(), "reason": reason,
                        "receiver": receiver, "method": method,
                        "amount_yocto": amount.as_yoctonear().to_string(),
                    }),
                );
                PromiseOrValue::Value(SpendResult { id: U64(id), status })
            }
            None => {
                // RESERVE first: take the amount out of the ledger before the async call.
                if let Some(policy) = self.policies.get_mut(&key) {
                    policy.last_request_id = request_id;
                    policy.balance = policy.balance.saturating_sub(amount);
                }
                let id = self.push_log(record);
                emit(
                    "request_approved",
                    json!({
                        "owner": owner, "agent": agent, "record_id": id.to_string(),
                        "request_id": rid.to_string(), "receiver": receiver, "method": method,
                        "amount_yocto": amount.as_yoctonear().to_string(),
                    }),
                );
                PromiseOrValue::Promise(
                    Promise::new(receiver)
                        .function_call(method, args.into_bytes(), amount, GAS_TARGET_CALL)
                        .then(
                            Self::ext(env::current_account_id())
                                .with_static_gas(GAS_CALLBACK)
                                .on_spend_result(owner, agent, U64(id), amount),
                        ),
                )
            }
        }
    }

    // ------------------------------------------------------------------------------------
    // Callbacks
    // ------------------------------------------------------------------------------------

    /// Keeps the spend if the target call succeeded, otherwise returns it to the ledger.
    #[private]
    pub fn on_spend_result(
        &mut self,
        owner: AccountId,
        agent: AccountId,
        record_id: U64,
        amount: NearToken,
    ) -> SpendResult {
        let succeeded = is_promise_success();
        if let Some(policy) = self.policies.get_mut(&(owner.clone(), agent.clone())) {
            if succeeded {
                policy.total_spent = policy.total_spent.saturating_add(amount);
            } else {
                policy.balance = policy.balance.saturating_add(amount);
            }
        }
        let status = if succeeded { RequestStatus::Executed } else { RequestStatus::FailedRefunded };
        self.set_status(record_id.0, status.clone());
        emit(
            if succeeded { "spend_executed" } else { "spend_refunded" },
            json!({
                "owner": owner, "agent": agent, "record_id": record_id.0.to_string(),
                "amount_yocto": amount.as_yoctonear().to_string(),
            }),
        );
        SpendResult { id: record_id, status }
    }

    /// Restores the ledger if the transfer to the owner failed.
    #[private]
    pub fn on_withdraw_result(&mut self, owner: AccountId, agent: AccountId, amount: NearToken) -> bool {
        let succeeded = is_promise_success();
        if !succeeded {
            if let Some(policy) = self.policies.get_mut(&(owner.clone(), agent.clone())) {
                policy.balance = policy.balance.saturating_add(amount);
            }
        }
        emit(
            if succeeded { "withdrawn" } else { "withdraw_failed" },
            json!({ "owner": owner, "agent": agent, "amount_yocto": amount.as_yoctonear().to_string() }),
        );
        succeeded
    }

    // ------------------------------------------------------------------------------------
    // Views
    // ------------------------------------------------------------------------------------

    pub fn get_policy(&self, owner: AccountId, agent: AccountId) -> Option<Policy> {
        self.policies.get(&(owner, agent)).cloned()
    }

    pub fn get_balance(&self, owner: AccountId, agent: AccountId) -> NearToken {
        self.policies
            .get(&(owner, agent))
            .map(|p| p.balance)
            .unwrap_or_else(|| NearToken::from_yoctonear(0))
    }

    pub fn list_policies(&self, from_index: u32, limit: u32) -> Vec<Policy> {
        self.policies
            .iter()
            .skip(from_index as usize)
            .take(limit.min(MAX_VIEW_LIMIT as u32) as usize)
            .map(|(_, p)| p.clone())
            .collect()
    }

    /// Total number of requests ever recorded.
    pub fn get_request_count(&self) -> U64 {
        U64(self.next_record_id)
    }

    /// Records with `id >= from_id`, oldest first. Only the last 200 records are kept.
    pub fn get_requests(&self, from_id: U64, limit: u64) -> Vec<RequestRecord> {
        let end = self.next_record_id;
        let oldest = end.saturating_sub(LOG_CAPACITY);
        let start = from_id.0.max(oldest);
        let stop = end.min(start.saturating_add(limit.min(MAX_VIEW_LIMIT)));
        (start..stop)
            .filter_map(|id| {
                self.log
                    .get((id % LOG_CAPACITY) as u32)
                    .filter(|r| r.id.0 == id)
                    .cloned()
            })
            .collect()
    }
}

// Internal helpers (not exported as contract methods).
impl Vault {
    fn policy_mut(&mut self, agent: &AccountId) -> &mut Policy {
        let key = (env::predecessor_account_id(), agent.clone());
        self.policies
            .get_mut(&key)
            .unwrap_or_else(|| env::panic_str("no policy for this owner and agent"))
    }

    fn validate_target(receiver: &AccountId, method: &str) {
        // Allowing the vault itself would let the agent reach #[private] callbacks.
        require!(receiver != &env::current_account_id(), "the vault cannot be an allowed target");
        require!(!method.is_empty(), "method name cannot be empty");
        require!(method.len() <= MAX_METHOD_LEN, "method name too long");
    }

    fn push_log(&mut self, mut record: RequestRecord) -> u64 {
        let id = self.next_record_id;
        self.next_record_id += 1;
        record.id = U64(id);
        if (self.log.len() as u64) < LOG_CAPACITY {
            self.log.push(record);
        } else {
            // Ring buffer: overwrite the oldest slot.
            let _ = self.log.replace((id % LOG_CAPACITY) as u32, record);
        }
        id
    }

    fn set_status(&mut self, id: u64, status: RequestStatus) {
        // If the record was already overwritten by newer ones, there is nothing to update.
        if let Some(record) = self.log.get_mut((id % LOG_CAPACITY) as u32) {
            if record.id.0 == id {
                record.status = status;
            }
        }
    }
}

/// NEP-297 style event log, easy to find in the explorer.
fn emit(event: &str, data: Value) {
    let payload = json!({
        "standard": "agentfence",
        "version": "1.0.0",
        "event": event,
        "data": data,
    });
    env::log_str(&format!("EVENT_JSON:{}", payload));
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::{testing_env, PromiseResult};

    const VAULT: &str = "vault.test.near";
    const OWNER: &str = "owner.test.near";
    const AGENT: &str = "agent.test.near";
    const NFT: &str = "nft.test.near";
    const EVIL: &str = "evil.test.near";

    fn acct(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn mnear(milli: u128) -> NearToken {
        NearToken::from_millinear(milli)
    }

    fn yocto1() -> NearToken {
        NearToken::from_yoctonear(1)
    }

    fn zero() -> NearToken {
        NearToken::from_yoctonear(0)
    }

    fn ctx(predecessor: &str, deposit: NearToken, gas_tgas: u64) -> VMContextBuilder {
        let mut b = VMContextBuilder::new();
        b.current_account_id(acct(VAULT))
            .predecessor_account_id(acct(predecessor))
            .signer_account_id(acct(predecessor))
            .attached_deposit(deposit)
            .prepaid_gas(Gas::from_tgas(gas_tgas));
        b
    }

    fn as_caller(predecessor: &str, deposit: NearToken) {
        testing_env!(ctx(predecessor, deposit, 100).build());
    }

    /// Vault with one policy: 5 NEAR budget, 1 NEAR per tx, only NFT.nft_mint allowed.
    fn setup() -> Vault {
        as_caller(OWNER, zero());
        let mut v = Vault::new();
        as_caller(OWNER, mnear(5000));
        v.create_policy(
            acct(AGENT),
            mnear(1000),
            vec![AllowedTarget { receiver: acct(NFT), method: "nft_mint".to_string() }],
        );
        v
    }

    fn spend(v: &mut Vault, id: u64, receiver: &str, method: &str, amount: NearToken) -> SpendResult {
        as_caller(AGENT, zero());
        match v.request_spend(
            acct(OWNER),
            U64(id),
            acct(receiver),
            method.to_string(),
            "{}".to_string(),
            amount,
        ) {
            PromiseOrValue::Value(r) => r,
            // For approved requests the Promise is built but not executed in unit tests.
            PromiseOrValue::Promise(_) => SpendResult {
                id: U64(v.next_record_id - 1),
                status: RequestStatus::Pending,
            },
        }
    }

    fn balance(v: &Vault) -> NearToken {
        v.get_balance(acct(OWNER), acct(AGENT))
    }

    fn callback(promise: PromiseResult) {
        testing_env!(
            ctx(VAULT, zero(), 100).build(),
            near_sdk::test_vm_config(),
            near_sdk::RuntimeFeesConfig::test(),
            Default::default(),
            vec![promise]
        );
    }

    #[test]
    fn approves_a_request_within_the_rules_and_reserves_the_amount() {
        let mut v = setup();
        let r = spend(&mut v, 1, NFT, "nft_mint", mnear(500));
        assert_eq!(r.status, RequestStatus::Pending);
        assert_eq!(balance(&v), mnear(4500)); // reserved immediately
        let policy = v.get_policy(acct(OWNER), acct(AGENT)).unwrap();
        assert_eq!(policy.last_request_id, U64(1));
        assert_eq!(policy.total_spent, zero()); // not spent until the callback confirms
        assert!(get_logs().iter().any(|l| l.contains("request_approved")));
    }

    #[test]
    fn rejects_amount_over_the_per_tx_limit() {
        let mut v = setup();
        let r = spend(&mut v, 1, NFT, "nft_mint", mnear(3000));
        assert_eq!(r.status, RequestStatus::Rejected(RejectReason::ExceedsTxLimit));
        assert_eq!(balance(&v), mnear(5000));
        assert!(get_logs().iter().any(|l| l.contains("request_rejected") && l.contains("ExceedsTxLimit")));
    }

    #[test]
    fn rejects_an_unknown_contract() {
        let mut v = setup();
        let r = spend(&mut v, 1, EVIL, "nft_mint", mnear(500));
        assert_eq!(r.status, RequestStatus::Rejected(RejectReason::TargetNotAllowed));
        assert_eq!(balance(&v), mnear(5000));
    }

    #[test]
    fn rejects_an_allowed_contract_with_a_disallowed_method() {
        let mut v = setup();
        let r = spend(&mut v, 1, NFT, "nft_transfer", mnear(500));
        assert_eq!(r.status, RequestStatus::Rejected(RejectReason::TargetNotAllowed));
    }

    #[test]
    fn rejects_a_replayed_request_id() {
        let mut v = setup();
        let first = spend(&mut v, 1, NFT, "nft_mint", mnear(500));
        assert_eq!(first.status, RequestStatus::Pending);
        let replay = spend(&mut v, 1, NFT, "nft_mint", mnear(500));
        assert_eq!(replay.status, RequestStatus::Rejected(RejectReason::Duplicate));
        assert_eq!(balance(&v), mnear(4500)); // only one reservation
    }

    #[test]
    #[should_panic(expected = "request_id must be last_request_id + 1")]
    fn panics_when_a_request_id_is_skipped() {
        let mut v = setup();
        spend(&mut v, 5, NFT, "nft_mint", mnear(500));
    }

    #[test]
    fn a_rejected_request_still_consumes_its_id() {
        let mut v = setup();
        let r = spend(&mut v, 1, EVIL, "nft_mint", mnear(500));
        assert_eq!(r.status, RequestStatus::Rejected(RejectReason::TargetNotAllowed));
        let again = spend(&mut v, 1, NFT, "nft_mint", mnear(500));
        assert_eq!(again.status, RequestStatus::Rejected(RejectReason::Duplicate));
        let next = spend(&mut v, 2, NFT, "nft_mint", mnear(500));
        assert_eq!(next.status, RequestStatus::Pending);
    }

    #[test]
    fn pause_blocks_requests_and_resume_allows_them_again() {
        let mut v = setup();
        as_caller(OWNER, zero());
        v.pause(acct(AGENT));
        let blocked = spend(&mut v, 1, NFT, "nft_mint", mnear(500));
        assert_eq!(blocked.status, RequestStatus::Rejected(RejectReason::Paused));
        as_caller(OWNER, yocto1());
        v.resume(acct(AGENT));
        let ok = spend(&mut v, 2, NFT, "nft_mint", mnear(500));
        assert_eq!(ok.status, RequestStatus::Pending);
    }

    #[test]
    fn rejects_when_the_remaining_budget_is_too_small() {
        let mut v = setup();
        for id in 1..=5 {
            assert_eq!(spend(&mut v, id, NFT, "nft_mint", mnear(1000)).status, RequestStatus::Pending);
        }
        assert_eq!(balance(&v), zero());
        let r = spend(&mut v, 6, NFT, "nft_mint", mnear(500));
        assert_eq!(r.status, RequestStatus::Rejected(RejectReason::InsufficientBudget));
    }

    #[test]
    fn two_requests_that_together_exceed_the_budget_cannot_both_pass() {
        // Reserve-before-call: the second request sees the reduced ledger even though the first
        // target call has not finished.
        let mut v = setup();
        as_caller(OWNER, yocto1());
        v.set_max_per_tx(acct(AGENT), mnear(4000));
        assert_eq!(spend(&mut v, 1, NFT, "nft_mint", mnear(3000)).status, RequestStatus::Pending);
        let second = spend(&mut v, 2, NFT, "nft_mint", mnear(3000));
        assert_eq!(second.status, RequestStatus::Rejected(RejectReason::InsufficientBudget));
        assert_eq!(balance(&v), mnear(2000));
    }

    #[test]
    fn callback_success_keeps_the_spend() {
        let mut v = setup();
        let r = spend(&mut v, 1, NFT, "nft_mint", mnear(500));
        callback(PromiseResult::Successful(vec![]));
        let out = v.on_spend_result(acct(OWNER), acct(AGENT), r.id, mnear(500));
        assert_eq!(out.status, RequestStatus::Executed);
        let policy = v.get_policy(acct(OWNER), acct(AGENT)).unwrap();
        assert_eq!(policy.balance, mnear(4500));
        assert_eq!(policy.total_spent, mnear(500));
        assert_eq!(v.get_requests(U64(0), 10)[0].status, RequestStatus::Executed);
    }

    #[test]
    fn callback_failure_refunds_the_ledger() {
        let mut v = setup();
        let r = spend(&mut v, 1, NFT, "nft_mint", mnear(500));
        assert_eq!(balance(&v), mnear(4500));
        callback(PromiseResult::Failed);
        let out = v.on_spend_result(acct(OWNER), acct(AGENT), r.id, mnear(500));
        assert_eq!(out.status, RequestStatus::FailedRefunded);
        let policy = v.get_policy(acct(OWNER), acct(AGENT)).unwrap();
        assert_eq!(policy.balance, mnear(5000));
        assert_eq!(policy.total_spent, zero());
        assert!(get_logs().iter().any(|l| l.contains("spend_refunded")));
    }

    #[test]
    #[should_panic(expected = "caller is not a registered agent for this owner")]
    fn panics_when_the_caller_is_not_a_registered_agent() {
        let mut v = setup();
        as_caller(EVIL, zero());
        v.request_spend(
            acct(OWNER), U64(1), acct(NFT), "nft_mint".to_string(), "{}".to_string(), mnear(500),
        );
    }

    #[test]
    #[should_panic(expected = "caller is not a registered agent for this owner")]
    fn a_stranger_cannot_use_someone_elses_policy_by_naming_the_owner() {
        let mut v = setup();
        as_caller("other.test.near", zero());
        v.request_spend(
            acct(OWNER), U64(1), acct(NFT), "nft_mint".to_string(), "{}".to_string(), mnear(500),
        );
    }

    #[test]
    #[should_panic(expected = "no policy for this owner and agent")]
    fn a_non_owner_cannot_change_the_policy() {
        let mut v = setup();
        as_caller(EVIL, zero());
        v.pause(acct(AGENT));
    }

    #[test]
    #[should_panic(expected = "Requires attached deposit of exactly 1 yoctoNEAR")]
    fn loosening_a_rule_needs_one_yocto() {
        let mut v = setup();
        as_caller(OWNER, zero());
        v.set_max_per_tx(acct(AGENT), mnear(4000));
    }

    #[test]
    #[should_panic(expected = "Requires attached deposit of exactly 1 yoctoNEAR")]
    fn withdraw_needs_one_yocto() {
        let mut v = setup();
        as_caller(OWNER, zero());
        v.withdraw(acct(AGENT), mnear(100));
    }

    #[test]
    fn withdraw_only_takes_ledger_funds_and_requests_afterwards_are_limited() {
        let mut v = setup();
        assert_eq!(spend(&mut v, 1, NFT, "nft_mint", mnear(1000)).status, RequestStatus::Pending);
        // 4 NEAR left in the ledger, 1 NEAR is reserved and cannot be withdrawn.
        as_caller(OWNER, yocto1());
        let _ = v.withdraw(acct(AGENT), mnear(4000));
        assert_eq!(balance(&v), zero());
        let r = spend(&mut v, 2, NFT, "nft_mint", mnear(500));
        assert_eq!(r.status, RequestStatus::Rejected(RejectReason::InsufficientBudget));
    }

    #[test]
    #[should_panic(expected = "amount exceeds the available budget")]
    fn withdraw_cannot_exceed_the_ledger() {
        let mut v = setup();
        spend(&mut v, 1, NFT, "nft_mint", mnear(1000));
        as_caller(OWNER, yocto1());
        let _ = v.withdraw(acct(AGENT), mnear(4500));
    }

    #[test]
    fn failed_withdraw_transfer_restores_the_ledger() {
        let mut v = setup();
        as_caller(OWNER, yocto1());
        let _ = v.withdraw(acct(AGENT), mnear(2000));
        assert_eq!(balance(&v), mnear(3000));
        callback(PromiseResult::Failed);
        let ok = v.on_withdraw_result(acct(OWNER), acct(AGENT), mnear(2000));
        assert!(!ok);
        assert_eq!(balance(&v), mnear(5000));
    }

    #[test]
    #[should_panic(expected = "the vault cannot be an allowed target")]
    fn the_vault_itself_cannot_be_allowlisted() {
        let mut v = setup();
        as_caller(OWNER, yocto1());
        v.add_allowed_target(acct(AGENT), acct(VAULT), "on_spend_result".to_string());
    }

    #[test]
    #[should_panic(expected = "the agent must be a different account than the owner")]
    fn the_owner_cannot_be_its_own_agent() {
        as_caller(OWNER, zero());
        let mut v = Vault::new();
        as_caller(OWNER, mnear(1000));
        v.create_policy(acct(OWNER), mnear(100), vec![]);
    }

    #[test]
    #[should_panic(expected = "attach at least 70 TGas")]
    fn too_little_gas_is_refused_up_front() {
        let mut v = setup();
        testing_env!(ctx(AGENT, zero(), 30).build());
        v.request_spend(
            acct(OWNER), U64(1), acct(NFT), "nft_mint".to_string(), "{}".to_string(), mnear(500),
        );
    }

    #[test]
    fn allowlist_changes_take_effect() {
        let mut v = setup();
        as_caller(OWNER, yocto1());
        v.add_allowed_target(acct(AGENT), acct(EVIL), "nft_mint".to_string());
        assert_eq!(spend(&mut v, 1, EVIL, "nft_mint", mnear(100)).status, RequestStatus::Pending);
        as_caller(OWNER, zero());
        v.remove_allowed_target(acct(AGENT), acct(EVIL), "nft_mint".to_string());
        assert_eq!(
            spend(&mut v, 2, EVIL, "nft_mint", mnear(100)).status,
            RequestStatus::Rejected(RejectReason::TargetNotAllowed)
        );
    }

    #[test]
    fn top_up_adds_to_the_budget() {
        let mut v = setup();
        as_caller(OWNER, mnear(250));
        v.top_up(acct(AGENT));
        let p = v.get_policy(acct(OWNER), acct(AGENT)).unwrap();
        assert_eq!(p.balance, mnear(5250));
        assert_eq!(p.total_deposited, mnear(5250));
    }

    #[test]
    fn log_is_a_ring_buffer_and_late_callbacks_do_not_panic() {
        let mut v = setup();
        // Every request is rejected (unknown target) so ids advance without reserving funds.
        for id in 1..=(LOG_CAPACITY + 50) {
            spend(&mut v, id, EVIL, "nft_mint", mnear(1));
        }
        assert_eq!(v.get_request_count(), U64(LOG_CAPACITY + 50));
        let all = v.get_requests(U64(0), 100);
        assert_eq!(all.first().unwrap().id, U64(50)); // oldest 50 were overwritten
        let last = v.get_requests(U64(LOG_CAPACITY + 49), 100);
        assert_eq!(last.len(), 1);
        // A callback for an overwritten record updates the ledger but not the log.
        callback(PromiseResult::Failed);
        let out = v.on_spend_result(acct(OWNER), acct(AGENT), U64(3), mnear(1));
        assert_eq!(out.status, RequestStatus::FailedRefunded);
        assert_eq!(balance(&v), mnear(5001));
    }
}