import "./style.css";
import { JsonRpcProvider } from "near-api-js";

// Override any of these in the URL: ?vault=...&owner=...&agent=...
const params = new URLSearchParams(location.search);
const RPC = params.get("rpc") ?? "https://test.rpc.fastnear.com";
const VAULT = params.get("vault") ?? "agentfence.tunga.testnet";
const OWNER = params.get("owner") ?? "tunga.testnet";
const AGENT = params.get("agent") ?? "spikeagent2.tunga.testnet";
const POLL_MS = 4000;
const LOG_SIZE = 25;

const provider = new JsonRpcProvider({ url: RPC });

type Status = string | { Rejected: string };
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

const YOCTO = 10n ** 24n;
function near(y: string | bigint, dp = 3): string {
  const v = BigInt(y);
  const frac = (v % YOCTO).toString().padStart(24, "0").slice(0, dp);
  return `${v / YOCTO}.${frac}`;
}

const REASONS: Record<string, string> = {
  Duplicate: "Duplicate request",
  Paused: "Vault paused",
  TargetNotAllowed: "Target not allowed",
  ExceedsTxLimit: "Over per-tx limit",
  InsufficientBudget: "Insufficient budget",
};

function statusPill(s: Status): string {
  if (typeof s === "object" && s !== null && "Rejected" in s) {
    const r = s.Rejected;
    return `<span class="pill bad">Rejected: ${esc(REASONS[r] ?? r)}</span>`;
  }
  if (s === "Executed") return `<span class="pill ok">Executed</span>`;
  if (s === "FailedRefunded") return `<span class="pill warn">Failed, refunded</span>`;
  return `<span class="pill warn">${esc(s)}</span>`;
}

const app = document.getElementById("app")!;
app.innerHTML = `
<main>
  <header>
    <div>
      <h1>AgentFence</h1>
      <p class="tag">The agent asks. The contract decides.</p>
    </div>
    <div class="live"><span class="dot" id="dot"></span><span id="updated">connecting...</span></div>
  </header>
  <section class="grid" id="stats"></section>
  <h2>Allowed targets</h2>
  <div class="allowed" id="allowed"></div>
  <h2>Request log</h2>
  <div class="tablewrap"><table>
    <thead><tr><th>#</th><th>Time</th><th>Request ID</th><th>Target</th><th>Amount</th><th>Result</th></tr></thead>
    <tbody id="log"></tbody>
  </table></div>
  <footer>
    Vault <a href="https://testnet.nearblocks.io/address/${esc(VAULT)}" target="_blank" rel="noreferrer">${esc(VAULT)}</a>
    &middot; Owner ${esc(OWNER)} &middot; Agent ${esc(AGENT)} &middot; NEAR testnet
    <div class="note">Prototype: protects only funds deposited in the vault. Not audited.</div>
  </footer>
</main>`;

const $ = (id: string) => document.getElementById(id)!;

function render(p: Policy | null, recs: Rec[]) {
  if (!p) {
    $("stats").innerHTML = `<div class="card"><div class="label">Policy</div><div class="value">Not found</div></div>`;
    $("allowed").innerHTML = "";
  } else {
    const stat = (label: string, value: string, unit = "") =>
      `<div class="card"><div class="label">${label}</div><div class="value">${value} <small>${unit}</small></div></div>`;
    $("stats").innerHTML =
      stat("Budget left", near(p.balance), "NEAR") +
      stat("Max per tx", near(p.max_per_tx), "NEAR") +
      stat("Total spent", near(p.total_spent), "NEAR") +
      `<div class="card"><div class="label">Status</div><div class="value">${
        p.paused ? `<span class="pill bad">Paused</span>` : `<span class="pill ok">Active</span>`
      }</div></div>`;
    $("allowed").innerHTML = p.allowed.length
      ? p.allowed.map((t) => `<code>${esc(t.receiver)} &rarr; ${esc(t.method)}</code>`).join("")
      : `<span class="note">No targets allowed: every request will be rejected.</span>`;
  }
  $("log").innerHTML = recs.length
    ? recs
        .map((r) => {
          const ms = Number(BigInt(r.timestamp_ns) / 1_000_000n);
          return `<tr>
            <td class="num">${esc(r.id)}</td>
            <td>${esc(new Date(ms).toLocaleTimeString())}</td>
            <td class="num">${esc(r.request_id)}</td>
            <td>${esc(r.receiver)} &rarr; ${esc(r.method)}</td>
            <td class="num">${near(r.amount)} NEAR</td>
            <td>${statusPill(r.status)}</td>
          </tr>`;
        })
        .join("")
    : `<tr><td colspan="6" class="empty">No requests yet.</td></tr>`;
}

async function refresh() {
  try {
    const [policy, count] = await Promise.all([
      view<Policy | null>("get_policy", { owner: OWNER, agent: AGENT }),
      view<string | number>("get_request_count", {}),
    ]);
    const n = Number(count);
    const from = Math.max(0, n - LOG_SIZE);
    const recs = n > 0 ? await view<Rec[]>("get_requests", { from_id: String(from), limit: LOG_SIZE }) : [];
    render(policy, [...recs].reverse());
    $("dot").className = "dot ok";
    $("updated").textContent = `live · updated ${new Date().toLocaleTimeString()}`;
  } catch (e) {
    $("dot").className = "dot err";
    $("updated").textContent = "can't reach the network, retrying...";
    console.error(e);
  }
}

refresh();
setInterval(refresh, POLL_MS);