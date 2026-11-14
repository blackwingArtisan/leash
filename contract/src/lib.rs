//! Leash: a NEAR vault that lets an AI agent spend, but only inside limits
//! its human set on-chain. The agent's key can call `pay`; it cannot change
//! its own policy, move funds anywhere off the allowlist, or exceed its caps.
//!
//! Every attempt is recorded. Over-cap payments become approval requests for
//! the owner. Payments to unknown payees are blocked, and repeated blocked
//! attempts trip an automatic freeze.

use near_sdk::json_types::U128;
use near_sdk::store::{IterableMap, IterableSet, Vector};
use near_sdk::{
    env, log, near, require, serde_json::json, AccountId, Gas, NearToken, PanicOnDefault, Promise,
    PromiseError,
};

const DAY_MS: u64 = 86_400_000;
/// Kept back from the vault so the contract can always pay for its own storage.
const RESERVE: u128 = 100_000_000_000_000_000_000_000; // 0.1 NEAR
const CALLBACK_GAS: Gas = Gas::from_tgas(10);

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug)]
pub struct Policy {
    /// Most the agent may spend on its own per UTC day, in yoctoNEAR.
    pub daily_cap: U128,
    /// Most a single payment may be before it needs the owner's approval.
    pub per_tx_cap: U128,
    /// The only accounts the agent may pay, each with its own daily cap.
    pub payees: Vec<(AccountId, U128)>,
    /// Blocked attempts per day that freeze the agent. 0 turns it off.
    pub tripwire: u32,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug)]
pub struct AgentState {
    pub policy: Policy,
    pub frozen: bool,
    pub frozen_reason: Option<String>,
    pub day: u64,
    pub spent_today: U128,
    pub payee_spent_today: Vec<(AccountId, U128)>,
    pub blocked_today: u32,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Paid,
    Blocked,
    Pending,
    Approved,
    Rejected,
    Refunded,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug)]
pub struct Record {
    pub id: u64,
    pub ts_ms: u64,
    pub agent: AccountId,
    pub to: AccountId,
    pub amount: U128,
    pub memo: String,
    pub outcome: Outcome,
    pub reason: Option<String>,
}

#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct PayResult {
    pub record_id: u64,
    pub outcome: Outcome,
    pub reason: Option<String>,
}

#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct AgentView {
    pub agent: AccountId,
    pub state: AgentState,
    pub remaining_today: U128,
}

/// What `pay` would do, before anything is written.
#[derive(Debug, PartialEq)]
enum Decision {
    Pay,
    Escalate(String),
    Block(String),
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Leash {
    owner: AccountId,
    agents: IterableMap<AccountId, AgentState>,
    history: Vector<Record>,
    pending: IterableSet<u64>,
}

fn today() -> u64 {
    env::block_timestamp_ms() / DAY_MS
}

fn payee_amount(list: &[(AccountId, U128)], who: &AccountId) -> Option<u128> {
    list.iter().find(|(a, _)| a == who).map(|(_, v)| v.0)
}

fn near_str(yocto: u128) -> String {
    format!("{}", NearToken::from_yoctonear(yocto).exact_amount_display())
}

impl AgentState {
    fn new(policy: Policy) -> Self {
        Self {
            policy,
            frozen: false,
            frozen_reason: None,
            day: today(),
            spent_today: U128(0),
            payee_spent_today: vec![],
            blocked_today: 0,
        }
    }

    /// Daily counters reset at UTC midnight. A freeze does not.
    fn roll_day(&mut self) {
        let d = today();
        if d != self.day {
            self.day = d;
            self.spent_today = U128(0);
            self.payee_spent_today.clear();
            self.blocked_today = 0;
        }
    }

    fn decide(&self, to: &AccountId, amount: u128, available: u128) -> Decision {
        if self.frozen {
            return Decision::Block(format!(
                "agent is frozen ({})",
                self.frozen_reason.clone().unwrap_or_default()
            ));
        }
        if amount == 0 {
            return Decision::Block("amount must be above zero".into());
        }
        let Some(payee_cap) = payee_amount(&self.policy.payees, to) else {
            return Decision::Block(format!("{to} is not on the allowlist"));
        };
        if amount > available {
            return Decision::Block("vault balance too low".into());
        }
        if amount > self.policy.per_tx_cap.0 {
            return Decision::Escalate(format!(
                "{} is over the per-payment cap of {}",
                near_str(amount),
                near_str(self.policy.per_tx_cap.0)
            ));
        }
        if self.spent_today.0 + amount > self.policy.daily_cap.0 {
            return Decision::Escalate(format!(
                "would pass the daily cap of {} ({} spent today)",
                near_str(self.policy.daily_cap.0),
                near_str(self.spent_today.0)
            ));
        }
        let payee_spent = payee_amount(&self.payee_spent_today, to).unwrap_or(0);
        if payee_spent + amount > payee_cap {
            return Decision::Escalate(format!(
                "would pass {to}'s daily cap of {}",
                near_str(payee_cap)
            ));
        }
        Decision::Pay
    }

    fn add_spend(&mut self, to: &AccountId, amount: u128) {
        self.spent_today = U128(self.spent_today.0 + amount);
        match self.payee_spent_today.iter_mut().find(|(a, _)| a == to) {
            Some((_, v)) => *v = U128(v.0 + amount),
            None => self.payee_spent_today.push((to.clone(), U128(amount))),
        }
    }

    fn undo_spend(&mut self, to: &AccountId, amount: u128) {
        self.spent_today = U128(self.spent_today.0.saturating_sub(amount));
        if let Some((_, v)) = self.payee_spent_today.iter_mut().find(|(a, _)| a == to) {
            *v = U128(v.0.saturating_sub(amount));
        }
    }
}

fn emit(event: &str, data: near_sdk::serde_json::Value) {
    log!(
        "EVENT_JSON:{}",
        json!({"standard": "leash", "version": "1.0.0", "event": event, "data": [data]})
    );
}

#[near]
impl Leash {
    #[init]
    pub fn new(owner: AccountId) -> Self {
        Self {
            owner,
            agents: IterableMap::new(b"a"),
            history: Vector::new(b"h"),
            pending: IterableSet::new(b"p"),
        }
    }

    // ---------- owner ----------

    pub fn set_agent(&mut self, agent: AccountId, policy: Policy) {
        self.only_owner();
        require!(!policy.payees.is_empty(), "an agent needs at least one payee");
        match self.agents.get_mut(&agent) {
            Some(state) => state.policy = policy,
            None => {
                self.agents.insert(agent.clone(), AgentState::new(policy));
            }
        }
        emit("policy_set", json!({"agent": agent}));
    }

    pub fn remove_agent(&mut self, agent: AccountId) {
        self.only_owner();
        self.agents.remove(&agent);
    }

    pub fn freeze(&mut self, agent: AccountId, reason: Option<String>) {
        self.only_owner();
        let reason = reason.unwrap_or_else(|| "frozen by owner".into());
        let state = self.agents.get_mut(&agent).expect("unknown agent");
        state.frozen = true;
        state.frozen_reason = Some(reason.clone());
        emit("agent_frozen", json!({"agent": agent, "reason": reason}));
    }

    pub fn unfreeze(&mut self, agent: AccountId) {
        self.only_owner();
        let state = self.agents.get_mut(&agent).expect("unknown agent");
        state.frozen = false;
        state.frozen_reason = None;
        state.blocked_today = 0;
        emit("agent_unfrozen", json!({"agent": agent}));
    }

    /// Clears today's spend and blocked counters, e.g. after a reviewed incident.
    pub fn reset_today(&mut self, agent: AccountId) {
        self.only_owner();
        let state = self.agents.get_mut(&agent).expect("unknown agent");
        state.day = today();
        state.spent_today = U128(0);
        state.payee_spent_today.clear();
        state.blocked_today = 0;
        emit("counters_reset", json!({"agent": agent}));
    }

    /// Sends a payment the agent asked for but was over its caps.
    pub fn approve(&mut self, record_id: u64) {
        self.only_owner();
        require!(self.pending.remove(&record_id), "no pending request with that id");
        let available = self.available();
        let rec = self.history.get_mut(record_id as u32).expect("missing record");
        require!(rec.amount.0 <= available, "vault balance too low");
        rec.outcome = Outcome::Approved;
        rec.reason = Some("approved by owner".into());
        let (to, amount) = (rec.to.clone(), rec.amount.0);
        emit("payment_approved", json!({"record_id": record_id, "to": to, "amount": U128(amount)}));
        self.transfer(record_id, to, amount, false);
    }

    pub fn reject(&mut self, record_id: u64) {
        self.only_owner();
        require!(self.pending.remove(&record_id), "no pending request with that id");
        let rec = self.history.get_mut(record_id as u32).expect("missing record");
        rec.outcome = Outcome::Rejected;
        rec.reason = Some("rejected by owner".into());
        emit("payment_rejected", json!({"record_id": record_id}));
    }

    pub fn withdraw(&mut self, amount: U128) -> Promise {
        self.only_owner();
        require!(amount.0 <= self.available(), "vault balance too low");
        Promise::new(self.owner.clone()).transfer(NearToken::from_yoctonear(amount.0))
    }

    /// Anyone can top up the vault.
    #[payable]
    pub fn fund(&mut self) {
        emit("funded", json!({"from": env::predecessor_account_id(), "amount": U128(env::attached_deposit().as_yoctonear())}));
    }

    // ---------- agent ----------

    /// The one thing an agent key can do. It never panics on a policy
    /// failure, so blocked attempts are kept on-chain for the owner to see.
    pub fn pay(&mut self, to: AccountId, amount: U128, memo: Option<String>) -> PayResult {
        let agent = env::predecessor_account_id();
        let available = self.available();
        let state = self.agents.get_mut(&agent).expect("caller is not a registered agent");
        state.roll_day();
        let memo: String = memo.unwrap_or_default().chars().take(200).collect();
        let record_id = self.history.len() as u64;
        let (outcome, reason) = match state.decide(&to, amount.0, available) {
            Decision::Pay => {
                state.add_spend(&to, amount.0);
                (Outcome::Paid, None)
            }
            Decision::Escalate(why) => (Outcome::Pending, Some(why)),
            Decision::Block(why) => {
                state.blocked_today += 1;
                let trip = state.policy.tripwire;
                if !state.frozen && trip > 0 && state.blocked_today >= trip {
                    state.frozen = true;
                    let r = format!("tripwire: {} blocked attempts today", state.blocked_today);
                    state.frozen_reason = Some(r.clone());
                    emit("agent_frozen", json!({"agent": agent, "reason": r}));
                }
                (Outcome::Blocked, Some(why))
            }
        };
        self.history.push(Record {
            id: record_id,
            ts_ms: env::block_timestamp_ms(),
            agent: agent.clone(),
            to: to.clone(),
            amount,
            memo: memo.clone(),
            outcome: outcome.clone(),
            reason: reason.clone(),
        });
        let data = json!({"record_id": record_id, "agent": agent, "to": to, "amount": amount, "memo": memo, "reason": reason});
        match outcome {
            Outcome::Paid => {
                emit("payment_sent", data);
                self.transfer(record_id, to, amount.0, true);
            }
            Outcome::Pending => {
                self.pending.insert(record_id);
                emit("approval_requested", data);
            }
            _ => emit("payment_blocked", data),
        }
        PayResult { record_id, outcome, reason }
    }

    #[private]
    pub fn on_transfer(
        &mut self,
        record_id: u64,
        autonomous: bool,
        #[callback_result] result: Result<(), PromiseError>,
    ) {
        if result.is_ok() {
            return;
        }
        let rec = self.history.get_mut(record_id as u32).expect("missing record");
        rec.outcome = Outcome::Refunded;
        rec.reason = Some("transfer failed; funds stayed in the vault".into());
        let (agent, to, amount, ts) = (rec.agent.clone(), rec.to.clone(), rec.amount.0, rec.ts_ms);
        if autonomous {
            if let Some(state) = self.agents.get_mut(&agent) {
                if state.day == ts / DAY_MS {
                    state.undo_spend(&to, amount);
                }
            }
        }
        emit("payment_refunded", json!({"record_id": record_id}));
    }

    // ---------- views ----------

    pub fn get_owner(&self) -> AccountId {
        self.owner.clone()
    }

    pub fn vault_balance(&self) -> U128 {
        U128(self.available())
    }

    pub fn get_agent(&self, agent: AccountId) -> Option<AgentView> {
        self.agents.get(&agent).map(|s| Self::view(&agent, s))
    }

    pub fn list_agents(&self) -> Vec<AgentView> {
        self.agents.iter().map(|(a, s)| Self::view(a, s)).collect()
    }

    /// What `pay` would do right now, without doing it. Agents call this
    /// first to plan; it is advisory, `pay` re-checks everything.
    pub fn check_payment(&self, agent: AccountId, to: AccountId, amount: U128) -> PayResult {
        let Some(state) = self.agents.get(&agent) else {
            return PayResult { record_id: 0, outcome: Outcome::Blocked, reason: Some("not a registered agent".into()) };
        };
        let mut s = state.clone();
        s.roll_day();
        let (outcome, reason) = match s.decide(&to, amount.0, self.available()) {
            Decision::Pay => (Outcome::Paid, None),
            Decision::Escalate(r) => (Outcome::Pending, Some(r)),
            Decision::Block(r) => (Outcome::Blocked, Some(r)),
        };
        PayResult { record_id: self.history.len() as u64, outcome, reason }
    }

    /// Newest first.
    pub fn get_history(&self, limit: Option<u32>) -> Vec<Record> {
        let n = self.history.len();
        let limit = limit.unwrap_or(50).min(200);
        (0..n).rev().take(limit as usize).filter_map(|i| self.history.get(i).cloned()).collect()
    }

    pub fn get_pending(&self) -> Vec<Record> {
        self.pending.iter().filter_map(|id| self.history.get(*id as u32).cloned()).collect()
    }
}

impl Leash {
    fn only_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "only the owner can do this");
    }

    fn available(&self) -> u128 {
        let storage = env::storage_byte_cost().as_yoctonear() * env::storage_usage() as u128;
        env::account_balance().as_yoctonear().saturating_sub(storage + RESERVE)
    }

    fn view(agent: &AccountId, s: &AgentState) -> AgentView {
        let mut s = s.clone();
        s.roll_day();
        let remaining = s.policy.daily_cap.0.saturating_sub(s.spent_today.0);
        AgentView { agent: agent.clone(), state: s, remaining_today: U128(remaining) }
    }

    fn transfer(&self, record_id: u64, to: AccountId, amount: u128, autonomous: bool) {
        let _ = Promise::new(to)
            .transfer(NearToken::from_yoctonear(amount))
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(CALLBACK_GAS)
                    .on_transfer(record_id, autonomous),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::testing_env;

    const NEAR: u128 = 1_000_000_000_000_000_000_000_000;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn ctx(caller: &str, ts_ms: u64) {
        let mut b = VMContextBuilder::new();
        b.current_account_id(acc("vault.testnet"))
            .predecessor_account_id(acc(caller))
            .account_balance(NearToken::from_near(50))
            .block_timestamp(ts_ms * 1_000_000);
        testing_env!(b.build());
    }

    fn setup() -> Leash {
        ctx("owner.testnet", 1_000);
        let mut c = Leash::new(acc("owner.testnet"));
        c.set_agent(
            acc("agent.testnet"),
            Policy {
                daily_cap: U128(5 * NEAR),
                per_tx_cap: U128(2 * NEAR),
                payees: vec![(acc("api.testnet"), U128(3 * NEAR)), (acc("hosting.testnet"), U128(5 * NEAR))],
                tripwire: 3,
            },
        );
        c
    }

    #[test]
    fn pays_inside_limits() {
        let mut c = setup();
        ctx("agent.testnet", 2_000);
        let r = c.pay(acc("api.testnet"), U128(NEAR), Some("credits".into()));
        assert_eq!(r.outcome, Outcome::Paid);
        assert_eq!(c.get_agent(acc("agent.testnet")).unwrap().remaining_today, U128(4 * NEAR));
        assert!(get_logs().iter().any(|l| l.contains("payment_sent")));
    }

    #[test]
    fn blocks_unknown_payee_and_records_it() {
        let mut c = setup();
        ctx("agent.testnet", 2_000);
        let r = c.pay(acc("attacker.testnet"), U128(NEAR), None);
        assert_eq!(r.outcome, Outcome::Blocked);
        assert!(r.reason.unwrap().contains("not on the allowlist"));
        assert_eq!(c.get_history(None)[0].outcome, Outcome::Blocked);
    }

    #[test]
    fn over_per_tx_cap_waits_for_owner() {
        let mut c = setup();
        ctx("agent.testnet", 2_000);
        let r = c.pay(acc("hosting.testnet"), U128(3 * NEAR), None);
        assert_eq!(r.outcome, Outcome::Pending);
        assert_eq!(c.get_pending().len(), 1);
        ctx("owner.testnet", 3_000);
        c.approve(r.record_id);
        assert!(c.get_pending().is_empty());
        assert_eq!(c.get_history(None)[0].outcome, Outcome::Approved);
    }

    #[test]
    fn daily_and_payee_caps_escalate() {
        let mut c = setup();
        ctx("agent.testnet", 2_000);
        assert_eq!(c.pay(acc("api.testnet"), U128(2 * NEAR), None).outcome, Outcome::Paid);
        // api.testnet's own cap is 3 NEAR
        let r = c.pay(acc("api.testnet"), U128(2 * NEAR), None);
        assert_eq!(r.outcome, Outcome::Pending);
        assert!(r.reason.unwrap().contains("api.testnet"));
        assert_eq!(c.pay(acc("hosting.testnet"), U128(2 * NEAR), None).outcome, Outcome::Paid);
        // 4 spent, daily cap 5
        let r = c.pay(acc("hosting.testnet"), U128(2 * NEAR), None);
        assert_eq!(r.outcome, Outcome::Pending);
        assert!(r.reason.unwrap().contains("daily cap"));
    }

    #[test]
    fn counters_reset_next_day() {
        let mut c = setup();
        ctx("agent.testnet", 2_000);
        c.pay(acc("hosting.testnet"), U128(2 * NEAR), None);
        c.pay(acc("hosting.testnet"), U128(2 * NEAR), None);
        ctx("agent.testnet", 2_000 + DAY_MS);
        assert_eq!(c.get_agent(acc("agent.testnet")).unwrap().remaining_today, U128(5 * NEAR));
        assert_eq!(c.pay(acc("hosting.testnet"), U128(2 * NEAR), None).outcome, Outcome::Paid);
    }

    #[test]
    fn owner_can_reset_today() {
        let mut c = setup();
        ctx("agent.testnet", 2_000);
        c.pay(acc("hosting.testnet"), U128(2 * NEAR), None);
        ctx("owner.testnet", 3_000);
        c.reset_today(acc("agent.testnet"));
        assert_eq!(c.get_agent(acc("agent.testnet")).unwrap().remaining_today, U128(5 * NEAR));
    }

    #[test]
    fn tripwire_freezes_after_repeated_blocks() {
        let mut c = setup();
        ctx("agent.testnet", 2_000);
        for _ in 0..3 {
            assert_eq!(c.pay(acc("attacker.testnet"), U128(NEAR), None).outcome, Outcome::Blocked);
        }
        let a = c.get_agent(acc("agent.testnet")).unwrap();
        assert!(a.state.frozen);
        assert!(a.state.frozen_reason.unwrap().contains("tripwire"));
        // even an allowed payment is now refused
        let r = c.pay(acc("api.testnet"), U128(NEAR), None);
        assert_eq!(r.outcome, Outcome::Blocked);
        assert!(r.reason.unwrap().contains("frozen"));
        // freeze survives a new day
        ctx("agent.testnet", 2_000 + DAY_MS);
        assert!(c.get_agent(acc("agent.testnet")).unwrap().state.frozen);
        ctx("owner.testnet", 3_000 + DAY_MS);
        c.unfreeze(acc("agent.testnet"));
        ctx("agent.testnet", 4_000 + DAY_MS);
        assert_eq!(c.pay(acc("api.testnet"), U128(NEAR), None).outcome, Outcome::Paid);
    }

    #[test]
    fn check_payment_matches_pay_without_writing() {
        let mut c = setup();
        ctx("agent.testnet", 2_000);
        let p = c.check_payment(acc("agent.testnet"), acc("attacker.testnet"), U128(NEAR));
        assert_eq!(p.outcome, Outcome::Blocked);
        assert!(c.get_history(None).is_empty());
        assert_eq!(c.pay(acc("attacker.testnet"), U128(NEAR), None).outcome, Outcome::Blocked);
    }

    #[test]
    #[should_panic(expected = "only the owner")]
    fn agent_cannot_raise_its_own_limits() {
        let mut c = setup();
        ctx("agent.testnet", 2_000);
        c.set_agent(
            acc("agent.testnet"),
            Policy { daily_cap: U128(1000 * NEAR), per_tx_cap: U128(1000 * NEAR), payees: vec![(acc("attacker.testnet"), U128(1000 * NEAR))], tripwire: 0 },
        );
    }

    #[test]
    #[should_panic(expected = "only the owner")]
    fn agent_cannot_approve_its_own_request() {
        let mut c = setup();
        ctx("agent.testnet", 2_000);
        let r = c.pay(acc("hosting.testnet"), U128(3 * NEAR), None);
        c.approve(r.record_id);
    }

    #[test]
    #[should_panic(expected = "not a registered agent")]
    fn strangers_cannot_pay() {
        let mut c = setup();
        ctx("random.testnet", 2_000);
        c.pay(acc("api.testnet"), U128(NEAR), None);
    }

    #[test]
    fn failed_transfer_rolls_back_spend() {
        let mut c = setup();
        ctx("agent.testnet", 2_000);
        let r = c.pay(acc("api.testnet"), U128(NEAR), None);
        ctx("vault.testnet", 2_500);
        c.on_transfer(r.record_id, true, Err(PromiseError::Failed));
        assert_eq!(c.get_history(None)[0].outcome, Outcome::Refunded);
        assert_eq!(c.get_agent(acc("agent.testnet")).unwrap().remaining_today, U128(5 * NEAR));
    }
}
