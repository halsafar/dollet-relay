import { describe, expect, it } from 'vitest';

import { ApiError, toApiError } from './errors.js';

describe('toApiError', () => {
  it('reads the one shape the server emits', () => {
    // Every handler answers through one `IntoResponse`, and it emits
    // `{"detail": ...}` and nothing else.
    const error = toApiError(403, { detail: 'You do not have permission.' });
    expect(error.message).toBe('You do not have permission.');
    expect(error.status).toBe(403);
    expect(error.isAuthError).toBe(true);
  });

  it('falls back for a body with no usable detail', () => {
    // Rather than digging for field arrays the server never sends.
    expect(toApiError(400, { username: ['taken'] }).message).toBe(
      'The request was rejected.',
    );
    expect(toApiError(400, { detail: '   ' }).message).toBe('The request was rejected.');
  });

  it('keeps the body for a caller that needs the original', () => {
    const body = { detail: 'nope', extra: 1 };
    expect(toApiError(400, body).payload).toBe(body);
  });

  it('accepts a bare string body', () => {
    expect(toApiError(500, 'boom').message).toBe('boom');
  });

  it('does not put a reverse proxy HTML error page in the message', () => {
    const page = '<html><body><h1>502 Bad Gateway</h1></body></html>';
    const error = toApiError(502, page);

    expect(error.message).toBe('The server is unreachable.');
    expect(error.payload).toBe(page);
  });

  it('falls back to a status-specific message for an empty body', () => {
    expect(toApiError(404, null).message).toBe('Not found.');
    expect(toApiError(418, null).message).toBe('Request failed (418).');
  });

  it('treats a status of 0 as a network failure', () => {
    const error = new ApiError('Could not reach the server.', { status: 0 });
    expect(error.isNetworkError).toBe(true);
    expect(error.isAuthError).toBe(false);
  });
});
