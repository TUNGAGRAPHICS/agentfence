import { readFileSync } from "node:fs";
import { Account, JsonRpcProvider, KeyPairSigner, nearToYocto, teraToGas } from "near-api-js";

const RPC = process.env.NEAR_RPC ?? "https://test.rpc.fastnear.com";
const VAULT = process.env.VAULT ?? "agentfence.tunga.testnet";
const OWNER = process.env.OWNER ?? "tunga.testnet";
const AGENT = process.env.AGENT ?? "spikeagent2.tunga.testnet";
const NFT = process.env.NFT ?? "agentnft.tunga.testnet";

// The agent signs with a function-call key that can only call request_spend on the vault.
function loadKey(): `ed25519:${string}` {
  if (process.env.AGENT_PRIVATE_KEY) return process.env.AGENT_PRIVATE_KEY as `ed25519:${string}`;
  const file = process.env.AGENT_KEY_FILE;
  if (file) return JSON.parse(readFileSync(file, "utf8")).private_key;
  throw new Error("Set AGENT_KEY_FILE (path to the key .json) or AGENT_PRIVATE_KEY");
}

const provider = new JsonRpcProvider({ url: RPC });
const agent = new Account(AGENT, provider, KeyPairSigner.fromSecretKey(loadKey()));

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function nextRequestId(): Promise<string> {
  const policy: any = await provider.callFunction({
    contractId: VAULT,
    method: "get_policy",
    args: { owner: OWNER, agent: AGENT },
  });
  return (BigInt(policy.last_request_id) + 1n).toString();
}

async function requestSpend(
  label: string,
  o: { receiver: string; method: string; args: object; amountNear: string; requestId?: string },
) {
  const id = o.requestId ?? (await nextRequestId());
  console.log(`\n> ${label}`);
  console.log(`  request ${id}: ${o.amountNear} NEAR -> ${o.receiver}.${o.method}`);
  try {
    const res: any = await agent.callFunction({
      contractId: VAULT,
      methodName: "request_spend",
      args: {
        owner: OWNER,
        request_id: id,
        receiver: o.receiver,
        method: o.method,
        args: JSON.stringify(o.args),
        amount: nearToYocto(o.amountNear).toString(),
      },
      gas: teraToGas("100"),
    });
    console.log("  vault says:", JSON.stringify(res?.status ?? res));
  } catch (e: any) {
    console.log("  call failed:", e?.message ?? e);
  }
}

const mintArgs = () => ({ token_id: `agent-${Date.now()}`, receiver_id: OWNER });

const scenarios: Record<string, () => Promise<void>> = {
  good: () =>
    requestSpend("Normal request: mint an NFT at the listed price", {
      receiver: NFT, method: "nft_mint", args: mintArgs(), amountNear: "0.5",
    }),
  overlimit: () =>
    requestSpend("Manipulated agent: tries to pay 3 NEAR", {
      receiver: NFT, method: "nft_mint", args: mintArgs(), amountNear: "3",
    }),
  unknown: () =>
    requestSpend("Manipulated agent: pays an unknown contract", {
      receiver: "attacker.testnet", method: "ft_transfer", args: {}, amountNear: "0.1",
    }),
  replay: () =>
    requestSpend("Replay: re-sends an old request id", {
      receiver: NFT, method: "nft_mint", args: mintArgs(), amountNear: "0.5", requestId: "1",
    }),
  // The agent tries to skip the vault and call the NFT directly with its own key.
  bypass: async () => {
    console.log("\n> Bypass attempt: agent calls the NFT directly, skipping the vault");
    try {
      await agent.callFunction({
        contractId: NFT,
        methodName: "nft_mint",
        args: { token_id: "stolen", receiver_id: AGENT },
        gas: teraToGas("30"),
      });
      console.log("  !! bypass worked. This should never happen.");
    } catch (e: any) {
      console.log("  blocked by the network:", e?.message ?? e);
    }
  },
};

const mode = process.argv[2] ?? "demo";
if (mode === "demo") {
  for (const name of ["good", "overlimit", "unknown", "replay", "bypass"]) {
    await scenarios[name]();
    await sleep(2000);
  }
} else if (scenarios[mode]) {
  await scenarios[mode]();
} else {
  console.log("modes: demo | good | overlimit | unknown | replay | bypass");
}