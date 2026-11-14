// One-time testnet setup: creates the vault, agent and merchant accounts under
// the owner account, deploys Leash, sets the agent's policy and funds the vault.
// Usage: OWNER=<you>.testnet npm run setup
import fs from 'node:fs';
import path from 'node:path';
import { HERE, DEPLOYMENT, accountFor, call, newKey, saveKey, toYocto, view, toNear } from './near.mjs';

const owner = process.env.OWNER;
if (!owner) throw new Error('Set OWNER to a funded testnet account in ~/.near-credentials/testnet');
const attacker = process.env.ATTACKER || 'refund-desk.testnet';

const ownerAcc = accountFor(owner);
const ids = {
  vault: `vault.${owner}`,
  agent: `agent.${owner}`,
  apiCredits: `api-credits.${owner}`,
  hosting: `cloud-hosting.${owner}`,
};
const funding = { vault: '7', agent: '0.5', apiCredits: '0.05', hosting: '0.05' };

for (const [role, id] of Object.entries(ids)) {
  const kp = newKey();
  console.log(`creating ${id} with ${funding[role]} NEAR`);
  await ownerAcc.createSubAccount({
    accountOrPrefix: id.split('.')[0],
    publicKey: kp.getPublicKey(),
    nearToTransfer: toYocto(funding[role]),
  });
  saveKey(id, kp);
}

const wasm = fs.readFileSync(path.join(HERE, '..', 'contract', 'target', 'near', 'leash.wasm'));
const vaultAcc = accountFor(ids.vault);
console.log(`deploying Leash (${wasm.length} bytes) to ${ids.vault}`);
await vaultAcc.deployContract(new Uint8Array(wasm));
await call(vaultAcc, ids.vault, 'new', { owner });

const policy = {
  daily_cap: toYocto('1.5').toString(),
  per_tx_cap: toYocto('1').toString(),
  payees: [
    [ids.apiCredits, toYocto('1').toString()],
    [ids.hosting, toYocto('1.5').toString()],
  ],
  tripwire: 3,
};
await call(ownerAcc, ids.vault, 'set_agent', { agent: ids.agent, policy });

fs.writeFileSync(DEPLOYMENT, JSON.stringify({ network: 'testnet', owner, ...ids, attacker }, null, 2) + '\n');
console.log('vault balance:', toNear(await view(ids.vault, 'vault_balance')), 'NEAR');
console.log('agent policy:', JSON.stringify((await view(ids.vault, 'get_agent', { agent: ids.agent })).state.policy));
console.log(`wrote ${DEPLOYMENT}`);
