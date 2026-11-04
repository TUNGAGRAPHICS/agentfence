# AgentFence

**An onchain spending firewall for AI agents, built on NEAR.**

> The agent asks. The contract decides.

Built during the NEAR Hackathon (1–14 Nov 2026). Track 06: AI Security & Defense. Also relevant to Track 07: Agent Economy.

## The problem

AI agents that can move money are one prompt injection away from draining a wallet. Most setups give the agent a key with full authority and hope the model behaves.

AgentFence flips that. The agent never holds the funds. A vault contract holds only the budget you allocate, and enforces your rules onchain. The agent can be wrong, manipulated or fully compromised, and the contract still says no.

## How it works

```
 Owner (your wallet)                      Agent account
   | create_policy / top_up / pause /       | holds NO funds
   | resume / withdraw / set rules          | has ONE function-call key:
   v                                        |   receiver = vault
 +-----------------------------+            |   method   = request_spend
 |  VAULT CONTRACT             | <----------+   small gas allowance
 |  - policy per (owner,agent) |
 |  - budget ledger            |   approved   +------------------+
 |  - request log + events     | -----------> | Target contract  |
 |  - replay protection        |   Promise    | (test NFT)       |
 +-----------------------------+ <----------- +------------------+
        ^        callback: success keeps the spend, failure refunds
        |
   Dashboard (reads views: policy, budget, request log)
```

1. The owner funds a **policy** for one agent: a budget, a per-transaction limit and an allowlist of `(contract, method)` pairs.
2. The agent holds a NEAR **function-call access key**. The network itself limits that key to one method (`request_spend`) on the vault, with no deposit attached. It cannot call anything else, including the target contract directly.
3. For every request the vault checks, in order: duplicate request id, paused, target not allowed, over the per-tx limit, insufficient budget. A failed check is **recorded onchain as a rejection** with a reason and an event, so it shows up in the explorer. It does not just revert.
4. An approved request **reserves** the amount first (the budget is reduced before the call), then calls the target. A callback keeps the spend on success and **refunds the ledger on failure**.

## Invariants

These are the properties the contract is designed to hold. Unit tests cover the vault logic.

1. A spend never exceeds `max_per_tx`.
2. Total spend never exceeds the deposited budget. Reserve-before-call makes concurrent requests safe.
3. Funds only go to allowlisted `(receiver, method)` pairs.
4. A request id is used at most once, and ids must increase in sequence, so replays cannot double-spend.
5. A failed target call refunds the ledger.
6. Only the registered agent can request a spend. Only the owner can change rules, pause, resume or withdraw.
7. While paused, no new spend is approved.
8. Withdraw can only take unreserved budget. In-flight funds are already out of the balance.

Sensitive owner methods (such as `resume` and `withdraw`) require exactly 1 yoctoNEAR attached. A function-call key can never attach a deposit, so a leaked limited key cannot call them.

## Testnet deployment

| Role | Account |
|---|---|
| Vault contract | `agentfence.tunga.testnet` |
| Test NFT (mint price 0.5 NEAR) | `agentnft.tunga.testnet` |
| Owner | `tunga.testnet` |
| Agent | `spikeagent2.tunga.testnet` |

## What the demo shows

Run against the live testnet contracts with `npm run agent -- demo`:

| Agent request | Result |
|---|---|
| Mint an NFT for 0.5 NEAR (allowed target, within limit) | `Executed` |
| Pay 3 NEAR | `Rejected: ExceedsTxLimit` |
| Pay an unknown contract | `Rejected: TargetNotAllowed` |
| Re-send an old request id | `Rejected: Duplicate` |
| Request more than the remaining budget | `Rejected: InsufficientBudget` |
| Owner pauses, agent asks again | `Rejected: Paused` |
| Mint with a deposit below the target's price | `FailedRefunded` (ledger restored) |
| Agent calls the NFT directly, skipping the vault | Blocked by the network: access key receiver does not match |

## Repository layout

```
contracts/vault/      Vault contract (Rust, near-sdk) with 26 unit tests
contracts/test-nft/   Minimal test NFT with a mint price (Rust, 3 unit tests)
agent/                TypeScript agent that signs with the restricted key
web/                  Dashboard (Vite + TypeScript, reads the vault views)
```

## Run it yourself

Prerequisites: Rust, `cargo-near`, the NEAR CLI (`near`), Node 20+, a funded testnet account.

**Contracts**

```
cd contracts/vault
cargo test --lib
cargo near build non-reproducible-wasm --no-abi
```

Deploy `target/near/vault.wasm` to a testnet account and call `new`. Do the same for `contracts/test-nft` with `new {"price": "500000000000000000000000"}`.

**Policy and agent key**

1. The owner calls `create_policy(agent, max_per_tx, allowed)` with the budget attached.
2. Add a function-call key to the agent account that is limited to `request_spend` on the vault.
3. Point the agent at the key file: `AGENT_KEY_FILE=<path to key json>`.

**Agent**

```
cd agent
npm install
npm run agent -- demo      # or: good | overlimit | unknown | replay | bypass
```

**Dashboard**

```
cd web
npm install
npm run dev
```

Open http://localhost:5173. Override the contracts with `?vault=...&owner=...&agent=...`.

## Limitations

This is a hackathon prototype.

- **Not audited.** Do not use it with real funds.
- It protects **only funds deposited into the vault**. It does not protect any other wallet the agent might control.
- The method allowlist does **not constrain call arguments**. An allowed `(contract, method)` pair can still be called with any arguments, within the amount limits.
- If a target refunds excess deposit, it goes to the vault account but is **not re-credited to the ledger**. The ledger never overstates what the agent can spend.
- The request log is a **ring buffer** (200 most recent records) to bound storage.
- Request ids must be sequential per agent, so an agent that runs requests in parallel has to coordinate its ids.
- The dashboard is **read-only**. Its owner controls generate ready-to-run CLI commands to copy, instead of signing through a wallet.
- Behaviour is verified by unit tests and live testnet runs. A scripted end-to-end sandbox suite is not included.

## Links

- Demo video: _TODO_
- X post: _TODO_