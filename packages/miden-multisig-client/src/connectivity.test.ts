import { describe, it, expect } from 'vitest';
import { GuardianHttpError, GuardianTransportError } from '@openzeppelin/guardian-client';
import { isGuardianUnreachable, isLikelyNetworkError, toUserFacingError } from './connectivity.js';

describe('isLikelyNetworkError', () => {
  it('flags codeless transport failures', () => {
    for (const m of [
      'Failed to fetch',
      'NetworkError when attempting to fetch resource',
      'Load failed',
      'The operation was aborted',
      'request timed out',
      'connection refused',
      'getaddrinfo ENOTFOUND guardian.example',
    ]) {
      expect(isLikelyNetworkError(new TypeError(m))).toBe(true);
    }
  });

  it('does not flag semantic errors', () => {
    expect(isLikelyNetworkError(new Error('account is paused'))).toBe(false);
    expect(isLikelyNetworkError(new Error('insufficient signatures'))).toBe(false);
  });
});

describe('toUserFacingError', () => {
  it('uses the server code + user-safe message when Guardian was reached', () => {
    const body = JSON.stringify({
      code: 'account_paused',
      message: "This account is paused and can't approve transactions right now.",
      meta: { retryable: false },
    });
    const result = toUserFacingError(new GuardianHttpError(409, 'Conflict', body));
    expect(result.code).toBe('account_paused');
    expect(result.userMessage).toContain('paused');
    expect(result.category).toBeUndefined();
  });

  it('classifies a codeless transport failure as connectivity', () => {
    const result = toUserFacingError(new TypeError('Failed to fetch'));
    expect(result.code).toBeUndefined();
    expect(result.category).toBe('unreachable');
    expect(result.userMessage).toContain("Can't reach Guardian");
    // The raw transport text is never the primary message.
    expect(result.userMessage).not.toContain('Failed to fetch');
  });

  it('classifies timeouts and aborts as the timeout category', () => {
    for (const m of ['request timed out', 'The operation was aborted']) {
      const result = toUserFacingError(new Error(m));
      expect(result.category).toBe('timeout');
      expect(result.userMessage).toContain("Can't reach Guardian");
    }
  });

  it.each([
    ['a refused connection', 'fetch failed'],
    ['a socket that died mid-response', 'terminated'],
  ])('classifies undici %s', (label, message) => {
    const result = toUserFacingError(new TypeError(message));

    expect(result.category).toBe('unreachable');
    expect(result.userMessage).toContain("Can't reach Guardian");
  });

  it('does not read a worker OOM as connectivity', () => {
    const err = new Error('Worker terminated due to reaching memory limit: JS heap out of memory');

    expect(toUserFacingError(err).category).toBeUndefined();
  });

  it('does not read a semantic failure as connectivity because of its cause', () => {
    const err = new Error('Guardian rejected the proposal: commitment mismatch');
    (err as { cause?: unknown }).cause = new Error('socket connection re-established');

    expect(toUserFacingError(err).category).toBeUndefined();
  });

  it('survives a self-referencing cause chain', () => {
    const err = new Error('boom') as Error & { cause?: unknown };
    err.cause = err;

    expect(() => toUserFacingError(err)).not.toThrow();
  });

  it('treats a reachable proxy 5xx with no Guardian body as connectivity', () => {
    const result = toUserFacingError(new GuardianHttpError(502, 'Bad Gateway', '<html>nope</html>'));
    expect(result.category).toBe('unreachable');
    expect(result.userMessage).toContain("Can't reach Guardian");
  });

  it('falls back to a generic message for unknown non-Guardian errors', () => {
    const result = toUserFacingError(new Error('totally unexpected'));
    expect(result.userMessage).toBe('Something went wrong. Please try again.');
    expect(result.userMessage).not.toContain('totally unexpected');
  });
});

describe('isGuardianUnreachable', () => {
  it.each([
    ['a transport failure the client raised', new GuardianTransportError('http://g', new TypeError('Failed to fetch'))],
    ['a codeless gateway 502', new GuardianHttpError(502, 'Bad Gateway', '<html>nope</html>')],
  ])('is true for %s', (label, error) => {
    expect(isGuardianUnreachable(error)).toBe(true);
  });

  it('is false for a bare transport error, which cannot have come from the client', () => {
    // GuardianHttpClient wraps every fetch rejection, so a loose TypeError here
    // is some other bug and must not be mistaken for the guardian being down.
    expect(isGuardianUnreachable(new TypeError('Failed to fetch'))).toBe(false);
  });

  it.each([
    ['a rate limit the guardian itself sent', new GuardianHttpError(429, 'Too Many Requests', '{"code":"rate_limited","message":"slow down","meta":{"retryable":true}}')],
    ['a conflict', new GuardianHttpError(409, 'Conflict', '{"error":"conflict"}')],
    ['a response this client could not decode', new TypeError("Cannot read properties of undefined (reading 'account_id')")],
    ['a rate limit with no envelope', new GuardianHttpError(429, 'Too Many Requests', 'slow down')],
  ])('is false for %s, which means the guardian answered', (label, error) => {
    expect(isGuardianUnreachable(error)).toBe(false);
  });
});
