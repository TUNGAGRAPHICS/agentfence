import "./style.css";
import { JsonRpcProvider } from "near-api-js";

// URL overrides: ?vault=...&owner=...&agent=...&nft=...
const params = new URLSearchParams(location.search);
const RPC = params.get("rpc") ?? "https://test.rpc.fastnear.com";
const VAULT = params.get("vault") ?? "agentfence.tunga.testnet";
const NFT = params.get("nft") ?? "agentnft.tunga.testnet";
const PINNED_OWNER = params.get("owner");
const DEFAULT_OWNER = "tunga.testnet";
const DEFAULT_AGENT = "spikeagent2.tunga.testnet";
const POLL_MS = 4000;
const LOG_SIZE = 60;

const store = {
  get: (k: string) => { try { return localStorage.getItem(k); } catch { return null; } },
  set: (k: string, v: string) => { try { localStorage.setItem(k, v); } catch { /* ignore */ } },
};

// The dashboard follows the connected wallet unless ?owner= pins it.
let OWNER = PINNED_OWNER ?? DEFAULT_OWNER;
let AGENT = params.get("agent") ?? store.get("agentfence.agent") ?? DEFAULT_AGENT;

const provider = new JsonRpcProvider({ url: RPC });

type Status = string | { Rejected: string };
type Kind = "executed" | "rejected" | "refunded" | "pending";
interface Policy {
  balance: string;
  max_per_tx: string;
  allowed: { receiver: string; method: string }[];
  paused: boolean;
  total_deposited: string;
  total_spent: string;
}
interface Rec {
  id: string;
  owner: string;
  agent: string;
  request_id: string;
  receiver: string;
  method: string;
  amount: string;
  status: Status;
  timestamp_ns: string;
}

const view = <T>(method: string, args: object) =>
  provider.callFunction({ contractId: VAULT, method, args }) as unknown as Promise<T>;

const esc = (s: unknown) =>
  String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
const $ = (id: string) => document.getElementById(id)!;
const val = (id: string) => ($(id) as HTMLInputElement).value.trim();
const short = (a: string) => (a.length > 26 ? `${a.slice(0, 10)}…${a.slice(-6)}` : a);

const YOCTO = 10n ** 24n;
function near(y: string | bigint, dp = 3): string {
  const v = BigInt(y);
  const frac = (v % YOCTO).toString().padStart(24, "0").slice(0, dp);
  return `${v / YOCTO}.${frac}`;
}
function toYocto(s: string): bigint {
  const [w, f = ""] = s.trim().split(".");
  return BigInt(w || "0") * YOCTO + BigInt((f + "0".repeat(24)).slice(0, 24));
}
const isAmount = (s: string) => /^\d+(\.\d+)?$/.test(s) && toYocto(s) > 0n;
function ago(ms: number): string {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 5) return "just now";
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

const REASONS: Record<string, string> = {
  Duplicate: "Duplicate request",
  Paused: "Vault paused",
  TargetNotAllowed: "Target not allowed",
  ExceedsTxLimit: "Over per-tx limit",
  InsufficientBudget: "Insufficient budget",
};
function kindOf(s: Status): Kind {
  if (typeof s === "object" && s !== null && "Rejected" in s) return "rejected";
  if (s === "Executed") return "executed";
  if (s === "FailedRefunded") return "refunded";
  return "pending";
}
function statusPill(s: Status): string {
  const k = kindOf(s);
  if (k === "rejected") return `<span class="pill bad">Rejected · ${esc(REASONS[(s as any).Rejected] ?? (s as any).Rejected)}</span>`;
  if (k === "executed") return `<span class="pill ok">Executed</span>`;
  if (k === "refunded") return `<span class="pill warn">Failed · refunded</span>`;
  return `<span class="pill warn">${esc(s)}</span>`;
}

// ---------- shell ----------
$("app").innerHTML = `
<main>
  <header class="top">
    <div class="brand">
      <svg class="logo" viewBox="0 0 24 24" width="30" height="30" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3l8 3v6c0 4.5-3.2 8-8 9-4.8-1-8-4.5-8-9V6z"/><path d="M9 12l2 2 4-4"/></svg>
      <div><h1>AgentFence</h1><p class="tag">The agent asks. The contract decides.</p></div>
    </div>
    <div class="right"><span class="net">NEAR testnet</span><div id="wallet"></div></div>
  </header>
  <div class="live"><span class="dot" id="dot"></span><span id="updated">connecting...</span></div>

  <section class="grid" id="stats"></section>

  <h2>Allowed targets</h2>
  <div class="allowed" id="allowed"></div>

  <h2>Owner controls</h2>
  <div class="card" id="owner"></div>

  <div class="loghead"><h2>Request log</h2><div class="filters" id="filters"></div></div>
  <div class="tablewrap"><table>
    <thead><tr><th>#</th><th>When</th><th>Req ID</th><th>Target</th><th>Amount</th><th>Result</th></tr></thead>
    <tbody id="log"></tbody>
  </table></div>

  <footer>
    <div>Vault <a href="https://testnet.nearblocks.io/address/${esc(VAULT)}" target="_blank" rel="noreferrer">${esc(VAULT)}</a>
    · NFT <a href="https://testnet.nearblocks.io/address/${esc(NFT)}" target="_blank" rel="noreferrer">${esc(NFT)}</a></div>
    <div id="who"></div>
    <div class="note">Prototype: protects only funds deposited in the vault. Not audited.</div>
  </footer>
</main>`;

// ---------- state ----------
let lastPolicy: Policy | null = null;
let lastRecs: Rec[] = [];
let fresh = new Set<string>();
const seen = new Set<string>();
let firstLoad = true;
let filter: "all" | "executed" | "rejected" | "refunded" = "all";
let ownerViewKey = "";

let connector: any = null;
let walletState: "loading" | "ready" | "unavailable" = "loading";
let account: string | null = null;
let busy = false;
let ownerMsg: { text: string; kind: "info" | "ok" | "err" } | null = null;

function renderWho() {
  $("who").innerHTML = `Viewing owner <b>${esc(short(OWNER))}</b> · agent <b>${esc(AGENT)}</b>`;
}

// ---------- render: stats / allowed / log ----------
function renderStats() {
  const p = lastPolicy;
  if (!p) {
    $("stats").innerHTML = `<div class="card wide"><div class="label">Policy</div><div class="value">None yet</div>
      <div class="sub">${esc(short(OWNER))} has no policy for ${esc(AGENT)}. Connect your wallet and create one below.</div></div>`;
    return;
  }
  const dep = BigInt(p.total_deposited);
  const pct = dep > 0n ? Number((BigInt(p.balance) * 1000n) / dep) / 10 : 0;
  const stat = (label: string, value: string, unit = "", extra = "") =>
    `<div class="card"><div class="label">${label}</div><div class="value">${value} <small>${unit}</small></div>${extra}</div>`;
  $("stats").innerHTML =
    stat("Budget left", near(p.balance), "NEAR",
      `<div class="bar"><span style="width:${Math.min(100, Math.max(0, pct))}%"></span></div><div class="sub">of ${near(p.total_deposited)} NEAR deposited</div>`) +
    stat("Max per tx", near(p.max_per_tx), "NEAR", `<div class="sub">hard cap per request</div>`) +
    stat("Total spent", near(p.total_spent), "NEAR", `<div class="sub">executed spends only</div>`) +
    `<div class="card"><div class="label">Status</div><div class="value">${
      p.paused ? `<span class="pill bad big">Paused</span>` : `<span class="pill ok big">Active</span>`
    }</div><div class="sub">${p.paused ? "all new spends are refused" : "agent may request within limits"}</div></div>`;
}

function renderAllowed() {
  const p = lastPolicy;
  $("allowed").innerHTML = !p
    ? `<span class="note">No policy yet.</span>`
    : p.allowed.length
      ? p.allowed.map((t) => `<code>${esc(t.receiver)} <b>→</b> ${esc(t.method)}</code>`).join("")
      : `<span class="note">No targets allowed: every request will be rejected.</span>`;
}

function renderLog() {
  const counts: Record<string, number> = { all: lastRecs.length, executed: 0, rejected: 0, refunded: 0 };
  for (const r of lastRecs) {
    const k = kindOf(r.status);
    if (k in counts) counts[k]++;
  }
  const labels: Record<string, string> = { all: "All", executed: "Executed", rejected: "Rejected", refunded: "Refunded" };
  $("filters").innerHTML = (["all", "executed", "rejected", "refunded"] as const)
    .map((f) => `<button class="chip ${filter === f ? "on" : ""}" data-f="${f}">${labels[f]} <span>${counts[f]}</span></button>`)
    .join("");

  const rows = lastRecs.filter((r) => filter === "all" || kindOf(r.status) === filter);
  $("log").innerHTML = rows.length
    ? rows
        .map((r) => {
          const ms = Number(BigInt(r.timestamp_ns) / 1_000_000n);
          return `<tr class="${fresh.has(r.id) ? "fresh" : ""}">
            <td class="num">${esc(r.id)}</td>
            <td title="${esc(new Date(ms).toLocaleString())}">${esc(ago(ms))}</td>
            <td class="num">${esc(r.request_id)}</td>
            <td><span class="tgt">${esc(r.receiver)}</span> <b class="arrow">→</b> ${esc(r.method)}</td>
            <td class="num">${near(r.amount)} NEAR</td>
            <td>${statusPill(r.status)}</td>
          </tr>`;
        })
        .join("")
    : `<tr><td colspan="6" class="empty">${lastRecs.length ? "Nothing matches this filter." : "No requests yet."}</td></tr>`;
}

$("filters").onclick = (e) => {
  const b = (e.target as HTMLElement).closest("button[data-f]") as HTMLButtonElement | null;
  if (!b) return;
  filter = b.dataset.f as typeof filter;
  renderLog();
};

// ---------- wallet ----------
function renderWallet() {
  const el = $("wallet");
  if (walletState === "unavailable") {
    el.innerHTML = `<span class="net warnnet" title="The wallet library did not load.">Wallet unavailable</span>`;
    return;
  }
  if (!account) {
    el.innerHTML = `<button class="primary" data-w="connect" ${walletState !== "ready" ? "disabled" : ""}>Connect wallet</button>`;
    return;
  }
  el.innerHTML = `<div class="acct"><span class="avatar">${esc(account[0]!.toUpperCase())}</span><span class="name" title="${esc(account)}">${esc(short(account))}</span><button class="ghost small" data-w="out">Disconnect</button></div>`;
}

function setAccount(a: string | null) {
  account = a;
  if (!PINNED_OWNER) OWNER = a ?? DEFAULT_OWNER;
  lastPolicy = null;
  lastRecs = [];
  seen.clear();
  firstLoad = true;
  renderWallet();
  renderWho();
  renderStats();
  renderAllowed();
  renderLog();
  renderOwner();
  refresh();
}

async function doConnect() {
  try {
    await connector.connect();
  } catch (err: any) {
    ownerMsg = { text: `Wallet connection cancelled or failed: ${friendly(err)}`, kind: "err" };
    renderOwner();
  }
}

$("wallet").onclick = async (e) => {
  const b = (e.target as HTMLElement).closest("button[data-w]") as HTMLButtonElement | null;
  if (!b || !connector) return;
  if (b.dataset.w === "connect") return doConnect();
  try {
    if (typeof connector.disconnect === "function") await connector.disconnect();
    else await (await connector.wallet()).signOut();
  } catch (err) {
    console.error(err);
  }
  ownerMsg = null;
  setAccount(null);
};

async function initWallet() {
  try {
    const { NearConnector } = await import("@fastnear/near-connect");
    connector = new NearConnector({ network: "testnet" } as any);
    connector.on("wallet:signIn", (e: any) => setAccount(e?.accounts?.[0]?.accountId ?? null));
    connector.on("wallet:signOut", () => setAccount(null));
    walletState = "ready";
    try {
      const w = await connector.wallet();
      const accts = await w.getAccounts();
      if (accts?.[0]?.accountId) {
        setAccount(accts[0].accountId);
        return;
      }
    } catch {
      /* not signed in yet */
    }
  } catch (e) {
    console.error("wallet library failed to load", e);
    walletState = "unavailable";
  }
  renderWallet();
  renderOwner();
}

async function ownerCall(method: string, args: object, deposit: string, gasTera = 30n) {
  const wallet = await connector.wallet();
  return wallet.signAndSendTransaction({
    receiverId: VAULT,
    actions: [
      {
        type: "FunctionCall",
        params: { methodName: method, args, gas: (gasTera * 10n ** 12n).toString(), deposit },
      },
    ],
  });
}

// ---------- owner controls ----------
const cli = (method: string, args: object, deposit: string, gas = "30") =>
  `near contract call-function as-transaction ${VAULT} ${method} json-args '${JSON.stringify(args).replace(/"/g, '\\"')}' prepaid-gas '${gas} Tgas' attached-deposit '${deposit}' sign-as ${OWNER} network-config testnet sign-with-keychain send`;

function cliBlock(): string {
  if (OWNER !== DEFAULT_OWNER) return "";
  return `<details class="cli"><summary>Prefer the terminal? Copy a command</summary>
    <p class="note">Signed by ${esc(OWNER)} with the NEAR CLI. Paste into PowerShell.</p>
    <div class="cmdrow"><span>Pause the agent</span><button class="danger small" data-c="pause">Copy</button></div>
    <div class="cmdrow"><span>Resume the agent</span><button class="small" data-c="resume">Copy</button></div>
    <div class="cmdrow"><span>Withdraw <input id="cli-amt" value="0.2" inputmode="decimal" /> NEAR</span><button class="small" data-c="withdraw">Copy</button></div>
    <p class="note" id="cli-msg"></p></details>`;
}

function msgHtml(): string {
  return ownerMsg ? `<p class="msg ${ownerMsg.kind}">${esc(ownerMsg.text)}</p>` : `<p class="msg"></p>`;
}

function createForm(): string {
  const dis = busy ? "disabled" : "";
  return `<div class="label">Create a policy for an agent</div>
    <p class="note">Your wallet deposits the budget into the vault. The agent can only spend within these rules.</p>
    <div class="fgrid">
      <label>Agent account<input id="c-agent" value="${esc(AGENT)}" /></label>
      <label>Budget (NEAR)<input id="c-budget" value="1" inputmode="decimal" /></label>
      <label>Max per request (NEAR)<input id="c-max" value="0.5" inputmode="decimal" /></label>
      <label>Allowed contract<input id="c-recv" value="${esc(NFT)}" /></label>
      <label>Allowed method<input id="c-meth" value="nft_mint" /></label>
    </div>
    <div class="btnrow"><button class="primary" data-act="create" ${dis}>Create policy</button></div>${msgHtml()}`;
}

function controls(): string {
  const dis = busy ? "disabled" : "";
  return `<div class="ownergrid">
      <div class="box"><div class="label">Emergency</div>
        <div class="btnrow"><button class="danger" data-act="pause" ${dis}>Pause agent</button><button data-act="resume" ${dis}>Resume</button></div></div>
      <div class="box"><div class="label">Withdraw unused budget</div>
        <div class="btnrow"><input id="w-amt" placeholder="0.2" inputmode="decimal" /><span class="unit">NEAR</span><button data-act="withdraw" ${dis}>Withdraw</button></div></div>
      <div class="box"><div class="label">Add budget</div>
        <div class="btnrow"><input id="t-amt" placeholder="1" inputmode="decimal" /><span class="unit">NEAR</span><button data-act="topup" ${dis}>Top up</button></div></div>
    </div>${msgHtml()}`;
}

function renderOwner() {
  ownerViewKey = `${walletState}|${account}|${OWNER}|${AGENT}|${!!lastPolicy}|${busy}`;
  const el = $("owner");
  let top: string;
  if (walletState === "loading") {
    top = `<p class="note">Loading wallet...</p>`;
  } else if (walletState === "unavailable") {
    top = `<p class="note">The wallet library could not load in this browser. Owner actions are available from the terminal below.</p>`;
  } else if (!account) {
    top = `<div class="prompt"><div><strong>Connect your wallet</strong><p class="note">Connect to manage your agent's policy, or create a new one. You are currently viewing the demo policy.</p></div>
      <button class="primary" data-w="connect">Connect wallet</button></div>${msgHtml()}`;
  } else if (account !== OWNER) {
    top = `<p class="note">Connected as <b>${esc(short(account))}</b>, but this view is pinned to owner <b>${esc(short(OWNER))}</b>. Owner controls are hidden for other accounts.</p>${msgHtml()}`;
  } else if (!lastPolicy) {
    top = createForm();
  } else {
    top = controls();
  }
  el.innerHTML = top + cliBlock();
}

function setMsg(text: string, kind: "info" | "ok" | "err") {
  ownerMsg = { text, kind };
  const m = $("owner").querySelector(".msg");
  if (m) {
    m.className = `msg ${kind}`;
    m.textContent = text;
  }
}
const friendly = (e: any) => {
  const t = String(e?.message ?? e);
  return /reject|cancel|denied|closed/i.test(t) ? "Cancelled in the wallet." : t;
};

$("owner").onclick = async (e) => {
  const t = e.target as HTMLElement;

  if (t.closest("button[data-w='connect']") && connector) return doConnect();

  const c = t.closest("button[data-c]") as HTMLButtonElement | null;
  if (c) {
    const which = c.dataset.c!;
    let text: string;
    if (which === "pause") text = cli("pause", { agent: AGENT }, "0 NEAR");
    else if (which === "resume") text = cli("resume", { agent: AGENT }, "1 yoctoNEAR");
    else {
      const v = val("cli-amt");
      if (!isAmount(v)) { $("cli-msg").textContent = "Enter an amount like 0.2"; return; }
      text = cli("withdraw", { agent: AGENT, amount: toYocto(v).toString() }, "1 yoctoNEAR", "60");
    }
    try {
      await navigator.clipboard.writeText(text);
      $("cli-msg").textContent = "Copied. Paste it into PowerShell.";
    } catch {
      $("cli-msg").textContent = text;
    }
    return;
  }

  const b = t.closest("button[data-act]") as HTMLButtonElement | null;
  if (!b || busy || !connector) return;
  const act = b.dataset.act!;
  try {
    let label = "";
    let run: () => Promise<unknown>;
    if (act === "pause") {
      label = "Pause";
      run = () => ownerCall("pause", { agent: AGENT }, "0");
    } else if (act === "resume") {
      label = "Resume";
      run = () => ownerCall("resume", { agent: AGENT }, "1");
    } else if (act === "withdraw") {
      const v = val("w-amt");
      if (!isAmount(v)) return setMsg("Enter an amount like 0.2", "err");
      if (lastPolicy && toYocto(v) > BigInt(lastPolicy.balance)) return setMsg("That is more than the budget left.", "err");
      label = "Withdraw";
      run = () => ownerCall("withdraw", { agent: AGENT, amount: toYocto(v).toString() }, "1", 60n);
    } else if (act === "topup") {
      const v = val("t-amt");
      if (!isAmount(v)) return setMsg("Enter an amount like 1", "err");
      label = "Top up";
      run = () => ownerCall("top_up", { agent: AGENT }, toYocto(v).toString());
    } else {
      const agent = val("c-agent"), budget = val("c-budget"), max = val("c-max"), recv = val("c-recv"), meth = val("c-meth");
      if (!/^[a-z0-9._-]{2,64}$/.test(agent)) return setMsg("Enter the agent's NEAR account, like agent.testnet", "err");
      if (!isAmount(budget) || !isAmount(max)) return setMsg("Budget and max per request must be amounts like 1 and 0.5", "err");
      if (!recv || !meth) return setMsg("Enter the allowed contract and method", "err");
      label = "Create policy";
      run = async () => {
        await ownerCall("create_policy", { agent, max_per_tx: toYocto(max).toString(), allowed: [{ receiver: recv, method: meth }] }, toYocto(budget).toString());
        AGENT = agent;
        store.set("agentfence.agent", agent);
      };
    }
        busy = true;
    renderOwner();
    setMsg(`${label}: waiting for wallet approval...`, "info");
    await run();
    busy = false;
    ownerMsg = { text: `${label} sent. Updating...`, kind: "ok" };
    renderWho();
    renderOwner();
    refresh();
    setTimeout(refresh, 2500);
    setTimeout(refresh, 7000);
  } catch (err: any) {
    busy = false;
    ownerMsg = { text: `Failed: ${friendly(err)}`, kind: "err" };
    renderOwner();
  }
};

// ---------- data loop ----------
async function refresh() {
  const owner = OWNER, agent = AGENT;
  try {
    const [policy, count] = await Promise.all([
      view<Policy | null>("get_policy", { owner, agent }),
      view<string | number>("get_request_count", {}),
    ]);
    const n = Number(count);
    const from = Math.max(0, n - LOG_SIZE);
    const recs = n > 0 ? await view<Rec[]>("get_requests", { from_id: String(from), limit: LOG_SIZE }) : [];
    if (owner !== OWNER || agent !== AGENT) return; // switched while loading
    lastPolicy = policy;
    lastRecs = recs.filter((r) => r.owner === owner && r.agent === agent).reverse();
    fresh = new Set<string>();
    if (!firstLoad) for (const r of lastRecs) if (!seen.has(r.id)) fresh.add(r.id);
    for (const r of lastRecs) seen.add(r.id);
    firstLoad = false;
    renderStats();
    renderAllowed();
    renderLog();
    if (ownerViewKey !== `${walletState}|${account}|${OWNER}|${AGENT}|${!!lastPolicy}|${busy}`) renderOwner();
    $("dot").className = "dot ok";
    $("updated").textContent = `live · updated ${new Date().toLocaleTimeString()}`;
  } catch (e) {
    $("dot").className = "dot err";
    $("updated").textContent = "can't reach the network, retrying...";
    console.error(e);
  }
}

renderWho();
renderWallet();
renderOwner();
refresh();
setInterval(refresh, POLL_MS);
initWallet();