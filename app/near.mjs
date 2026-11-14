// Shared NEAR helpers for the setup script, the agent and the owner server.
import { Account, FailoverRpcProvider, JsonRpcProvider, KeyPair, nearToYocto, yoctoToNear } from 'near-api-js';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const HERE = path.dirname(fileURLToPath(import.meta.url));
// Free testnet RPCs rate-limit (HTTP 429), so fail over between several.
export const RPCS = (process.env.NEAR_RPC || 'https://rpc.testnet.fastnear.com,https://test.rpc.fastnear.com,https://rpc.testnet.near.org').split(',');
export const provider = new FailoverRpcProvider(RPCS.map((url) => new JsonRpcProvider({ url })));
export const DEPLOYMENT = path.join(HERE, 'deployment.json');
const KEYCHAIN = path.join(os.homedir(), '.near-credentials', 'testnet');

export const toYocto = (near) => BigInt(nearToYocto(String(near)));
export const toNear = (yocto) => Number(yoctoToNear(BigInt(yocto), 4));

export function loadDeployment() {
  if (!fs.existsSync(DEPLOYMENT)) throw new Error('No deployment.json yet: run `npm run setup` first.');
  return JSON.parse(fs.readFileSync(DEPLOYMENT, 'utf8'));
}

// Keys live in the NEAR CLI's legacy keychain (~/.near-credentials), never in the repo.
export function secretFor(accountId) {
  const file = path.join(KEYCHAIN, `${accountId}.json`);
  return JSON.parse(fs.readFileSync(file, 'utf8')).private_key;
}

export function saveKey(accountId, keyPair) {
  fs.mkdirSync(KEYCHAIN, { recursive: true });
  const file = path.join(KEYCHAIN, `${accountId}.json`);
  fs.writeFileSync(
    file,
    JSON.stringify({ account_id: accountId, public_key: keyPair.getPublicKey().toString(), private_key: keyPair.toString() }),
    { mode: 0o600 },
  );
}

export const newKey = () => KeyPair.fromRandom('ed25519');

export function accountFor(accountId) {
  return new Account(accountId, provider, secretFor(accountId));
}

export function view(contractId, method, args = {}) {
  return provider.callFunction({ contractId, method, args });
}

export function call(account, contractId, methodName, args = {}, deposit = 0n) {
  return account.callFunction({ contractId, methodName, args, deposit, gas: 100_000_000_000_000n });
}
