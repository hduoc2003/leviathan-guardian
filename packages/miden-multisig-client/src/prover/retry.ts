import type { ProvenTransaction, TransactionResult, WasmWebClient } from '@miden-sdk/miden-sdk';
import type { ResolvedProverConfig } from './config.js';
import { isTransientProverError } from './errors.js';
import type { RetryRuntime } from '../retry/runtime.js';
import { productionRetryRuntime, retryTransient } from '../retry/runtime.js';

// Only proving is retried. A failed proof has submitted nothing, so retrying is
// free; widening the retry to cover submission would resend a transaction that
// may already have landed.
export async function proveWithRetry(
  client: WasmWebClient,
  execution: TransactionResult,
  config: ResolvedProverConfig,
  runtime: RetryRuntime = productionRetryRuntime,
): Promise<ProvenTransaction> {
  return retryTransient(
    async () => {
      const prover = config.createProver();
      return await client.proveTransaction(execution, prover ?? null);
    },
    config.maxAttempts,
    isTransientProverError,
    runtime,
  );
}
