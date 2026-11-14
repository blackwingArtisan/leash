// Swaps the agent's full-access key for a function-call key that can only
// call `pay` on the vault, with a small gas allowance. After this, even a
// stolen agent key can't move the agent's own NEAR or call anything else.
import { Account } from 'near-api-js';
import { accountFor, loadDeployment, newKey, provider, saveKey, secretFor, toYocto } from './near.mjs';

export async function scopeAgentKey(d) {
  const full = accountFor(d.agent);
  const oldPublic = (await import('near-api-js')).KeyPair.fromString(secretFor(d.agent)).getPublicKey();
  const kp = newKey();
  await full.addFunctionCallAccessKey({
    publicKey: kp.getPublicKey(),
    contractId: d.vault,
    methodNames: ['pay'],
    allowance: toYocto('0.25'),
  });
  // Delete the full-access key while it can still sign, then keep only the scoped key.
  await full.deleteKey(oldPublic);
  saveKey(d.agent, kp);
  const keys = await new Account(d.agent, provider).getAccessKeyList();
  return keys.keys.map((k) => k.access_key.permission);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  console.log(JSON.stringify(await scopeAgentKey(loadDeployment()), null, 2));
}
