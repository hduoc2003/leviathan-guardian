import type { AccountId, TransactionRequest, WasmWebClient } from '@miden-sdk/miden-sdk';
import type { ResolvedProverConfig } from './config.js';
import type { RetryRuntime } from '../retry/runtime.js';
import { proveWithRetry } from './retry.js';

export class ProverWorkflow {
  // Takes the caller's already-resolving raw client rather than deriving one:
  // `getRawMidenClient` needs the RPC endpoint on a cache miss, and this class
  // has no reason to know it.
  constructor(
    private readonly rawClient: Promise<WasmWebClient>,
    private readonly config: ResolvedProverConfig,
    private readonly runtime?: RetryRuntime,
  ) {}

  async submit(accountId: AccountId, request: TransactionRequest): Promise<void> {
    const raw = await this.rawClient;
    const execution = await raw.executeTransaction(accountId, request);
    const proof = await proveWithRetry(raw, execution, this.config, this.runtime);
    const submissionHeight = await raw.submitProvenTransaction(proof, execution);
    await raw.applyTransaction(execution, submissionHeight);
  }
}
