import { describe, expect, it } from 'vitest';

import { NetworkId } from '@miden-sdk/miden-sdk';

import { networkIdFromPrefix, parseAccountAddress, toAccountAddress } from './address.js';

const ACCOUNT_HEX = '0x7b7b7b7a7b7b7b017b7b7b7b7b7b7b';
// What the Rust `AccountId::to_bech32(NetworkId::Testnet)` gives for ACCOUNT_HEX.
const ACCOUNT_TESTNET = 'mtst1apahk7m60dahkqtm0dahk7mm0v5yjjjx';

describe('account addresses', () => {
  it('renders a hex id as the bech32 address Rust renders', () => {
    expect(toAccountAddress(ACCOUNT_HEX, NetworkId.testnet())).toBe(ACCOUNT_TESTNET);
    expect(toAccountAddress(ACCOUNT_HEX, networkIdFromPrefix('mtst'))).toBe(ACCOUNT_TESTNET);
  });

  it('parses a bech32 address of the expected network back to hex', () => {
    expect(parseAccountAddress(`  ${ACCOUNT_TESTNET}  `, NetworkId.testnet())).toBe(ACCOUNT_HEX);
  });

  it('rejects an address of another network', () => {
    const devnet = toAccountAddress(ACCOUNT_HEX, NetworkId.devnet());
    expect(() => parseAccountAddress(devnet, NetworkId.testnet())).toThrow(
      "is for network 'mdev', expected 'mtst'",
    );
  });

  it('accepts 0x hex as well', () => {
    expect(parseAccountAddress(` ${ACCOUNT_HEX} `, NetworkId.testnet())).toBe(ACCOUNT_HEX);
    expect(parseAccountAddress(ACCOUNT_HEX.replace('0x', '0X'), NetworkId.testnet())).toBe(
      ACCOUNT_HEX,
    );
    expect(() => parseAccountAddress('0xnope', NetworkId.testnet())).toThrow(
      "invalid account id '0xnope'",
    );
  });

  it('rejects anything else', () => {
    for (const input of ['faucet', '']) {
      expect(() => parseAccountAddress(input, NetworkId.testnet())).toThrow(
        'expected a bech32 address or 0x-prefixed hex',
      );
    }
  });
});
