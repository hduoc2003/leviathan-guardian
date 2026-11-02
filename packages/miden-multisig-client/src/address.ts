import { AccountId, Address, NetworkId } from '@miden-sdk/miden-sdk';

/** The network a bech32 prefix names, such as `mtst` or `mdev`. */
export function networkIdFromPrefix(prefix: string): NetworkId {
  switch (prefix) {
    case 'mm':
      return NetworkId.mainnet();
    case 'mtst':
      return NetworkId.testnet();
    case 'mdev':
      return NetworkId.devnet();
    default:
      return NetworkId.custom(prefix);
  }
}

/** A hex account id, such as the GUARDIAN API returns, as the bech32 address people read. */
export function toAccountAddress(accountIdHex: string, networkId: NetworkId): string {
  return Address.fromAccountId(AccountId.fromHex(accountIdHex)).toBech32(networkId);
}

/**
 * The hex account id a person entered as `0x` hex or as a bech32 address, which must
 * name `networkId`.
 */
export function parseAccountAddress(input: string, networkId: NetworkId): string {
  const address = input.trim();
  if (/^0x/i.test(address)) {
    try {
      return AccountId.fromHex(`0x${address.slice(2)}`).toString();
    } catch (error) {
      throw new Error(`invalid account id '${address}'`, { cause: error });
    }
  }
  let accountId: AccountId;
  try {
    accountId = AccountId.fromBech32(address);
  } catch (error) {
    throw new Error(
      `invalid account address '${address}': expected a bech32 address or 0x-prefixed hex`,
      { cause: error },
    );
  }
  const expected = Address.fromAccountId(accountId).toBech32(networkId);
  const prefix = address.slice(0, address.lastIndexOf('1'));
  const expectedPrefix = expected.slice(0, expected.lastIndexOf('1'));
  if (prefix !== expectedPrefix) {
    throw new Error(
      `account address '${address}' is for network '${prefix}', expected '${expectedPrefix}'`,
    );
  }
  return accountId.toString();
}
