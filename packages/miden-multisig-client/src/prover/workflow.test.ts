import type {
  AccountId,
  TransactionProver,
  TransactionRequest,
  WasmWebClient,
} from '@miden-sdk/miden-sdk';
import { describe, expect, it, vi } from 'vitest';
import type { ResolvedProverConfig } from './config.js';
import type { RetryRuntime } from '../retry/runtime.js';
import { ProverWorkflow } from './workflow.js';

function asType<T>(value: unknown): T {
  return value as T;
}

function rawClient(overrides: Record<string, unknown>) {
  return Promise.resolve(asType<WasmWebClient>({
    executeTransaction: vi.fn().mockResolvedValue({ marker: 'unchanged' }),
    proveTransaction: vi.fn().mockResolvedValue({}),
    submitProvenTransaction: vi.fn().mockResolvedValue(7),
    applyTransaction: vi.fn().mockResolvedValue({}),
    ...overrides,
  }));
}

describe('ProverWorkflow', () => {
  it('executes once, retries proof with fresh provers, submits once, and applies once', async () => {
    const transient = Object.assign(new Error('temporarily unavailable'), {
      code: 'Unavailable',
    });
    const executeTransaction = vi.fn().mockResolvedValue({ marker: 'unchanged' });
    const proveTransaction = vi
      .fn()
      .mockRejectedValueOnce(transient)
      .mockResolvedValueOnce({});
    const submitProvenTransaction = vi.fn().mockResolvedValue(7);
    const applyTransaction = vi.fn().mockResolvedValue({});
    const client = rawClient({
      executeTransaction,
      proveTransaction,
      submitProvenTransaction,
      applyTransaction,
    });
    const provers: TransactionProver[] = [];
    const config: ResolvedProverConfig = {
      kind: 'remote',
      url: 'https://prover.example/',
      maxAttempts: 2,
      createProver: () => {
        const prover = asType<TransactionProver>({});
        provers.push(prover);
        return prover;
      },
    };
    const runtime: RetryRuntime = {
      sleep: vi.fn().mockResolvedValue(undefined),
      unitRandom: () => 0.5,
    };
    const workflow = new ProverWorkflow(client, config, runtime);

    await workflow.submit(
      asType<AccountId>({}),
      asType<TransactionRequest>({}),
    );

    expect(executeTransaction).toHaveBeenCalledTimes(1);
    expect(proveTransaction).toHaveBeenCalledTimes(2);
    expect(provers).toHaveLength(2);
    expect(provers[0]).not.toBe(provers[1]);
    expect(submitProvenTransaction).toHaveBeenCalledTimes(1);
    expect(applyTransaction).toHaveBeenCalledTimes(1);
    expect(applyTransaction).toHaveBeenCalledWith({ marker: 'unchanged' }, 7);
    expect(runtime.sleep).toHaveBeenCalledTimes(1);
  });

  it('returns the final original error without sleeping after exhaustion', async () => {
    const first = Object.assign(new Error('unavailable'), { code: 'Unavailable' });
    const final = Object.assign(new Error('deadline exceeded'), {
      code: 'DeadlineExceeded',
    });
    const proveTransaction = vi
      .fn()
      .mockRejectedValueOnce(first)
      .mockRejectedValueOnce(final);
    const client = rawClient({ proveTransaction });
    const runtime: RetryRuntime = {
      sleep: vi.fn().mockResolvedValue(undefined),
      unitRandom: () => 0.5,
    };
    const workflow = new ProverWorkflow(
      client,
      {
        kind: 'remote',
        maxAttempts: 2,
        createProver: () => asType<TransactionProver>({}),
      },
      runtime,
    );

    await expect(
      workflow.submit(asType<AccountId>({}), asType<TransactionRequest>({})),
    ).rejects.toBe(final);
    expect(proveTransaction).toHaveBeenCalledTimes(2);
    expect(runtime.sleep).toHaveBeenCalledTimes(1);
  });

  it('never re-submits when submission fails with transient-looking wording', async () => {
    const rateLimited = Object.assign(new Error('Too Many Requests!'), {
      code: 'ResourceExhausted',
    });
    const proveTransaction = vi.fn().mockResolvedValue({});
    const submitProvenTransaction = vi.fn().mockRejectedValue(rateLimited);
    const applyTransaction = vi.fn().mockResolvedValue({});
    const client = rawClient({
      proveTransaction,
      submitProvenTransaction,
      applyTransaction,
    });
    const runtime: RetryRuntime = {
      sleep: vi.fn().mockResolvedValue(undefined),
      unitRandom: () => 0.5,
    };
    const workflow = new ProverWorkflow(
      client,
      {
        kind: 'remote',
        maxAttempts: 5,
        createProver: () => asType<TransactionProver>({}),
      },
      runtime,
    );

    await expect(
      workflow.submit(asType<AccountId>({}), asType<TransactionRequest>({})),
    ).rejects.toBe(rateLimited);
    expect(proveTransaction).toHaveBeenCalledTimes(1);
    expect(submitProvenTransaction).toHaveBeenCalledTimes(1);
    expect(applyTransaction).not.toHaveBeenCalled();
    expect(runtime.sleep).not.toHaveBeenCalled();
  });

  it('passes no prover when the config yields none', async () => {
    const proveTransaction = vi.fn().mockResolvedValue({});
    const client = rawClient({ proveTransaction });
    const workflow = new ProverWorkflow(client, {
      kind: 'injected',
      maxAttempts: 1,
      createProver: () => undefined,
    });

    await workflow.submit(asType<AccountId>({}), asType<TransactionRequest>({}));

    expect(proveTransaction).toHaveBeenCalledWith({ marker: 'unchanged' }, null);
  });
});
