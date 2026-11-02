import {
  networkIdFromPrefix,
  parseAccountAddress,
  toAccountAddress,
} from '@openzeppelin/miden-multisig-client';
import { LEVIATHAN_NETWORK_PREFIX } from '@/config';

export function displayAccountId(accountIdHex: string): string {
  return toAccountAddress(accountIdHex, networkIdFromPrefix(LEVIATHAN_NETWORK_PREFIX));
}

export function parseAccountInput(input: string): string {
  return parseAccountAddress(input, networkIdFromPrefix(LEVIATHAN_NETWORK_PREFIX));
}
