/**
 * One error type for every failure mode, so callers never have to distinguish
 * "the network died" from "the server said no" by inspecting shapes.
 */
export class ApiError extends Error {
  /**
   * @param {string} message
   * @param {{ status?: number, payload?: unknown, cause?: unknown }} [options]
   */
  constructor(message, options = {}) {
    super(message, { cause: options.cause });
    this.name = 'ApiError';
    /** HTTP status, or 0 when the request never reached the server. */
    this.status = options.status ?? 0;
    /** The parsed response body, for the rare caller that needs the original. */
    this.payload = options.payload ?? null;
  }

  get isNetworkError() {
    return this.status === 0;
  }

  get isAuthError() {
    return this.status === 401 || this.status === 403;
  }
}

const STATUS_FALLBACKS = {
  400: 'The request was rejected.',
  401: 'Your session has expired.',
  403: 'You do not have permission to do that.',
  404: 'Not found.',
  409: 'That conflicts with something that already exists.',
  429: 'Too many requests. Try again shortly.',
  500: 'The server hit an unexpected error.',
  502: 'The server is unreachable.',
  503: 'The server is unavailable.',
};

/**
 * Turns a failed response into an `ApiError`.
 *
 * Every error body carries `{"detail": "..."}`, from a single `IntoResponse`
 * for every handler, so `detail` is all this reads into the message. The
 * per-field contract is a flat `fields` map beside it —
 * `{"detail": "...", "fields": {"username": "already taken"}}`, one string per
 * field — which the pages that show it read straight off `payload`. Nothing
 * here unpacks per-field arrays or `non_field_errors`, because the API never
 * sends them.
 *
 * `fields` stays flat and optional: a form shows one line under a field, and
 * `detail` alone is always enough to render.
 *
 * @param {number} status
 * @param {unknown} body
 * @returns {ApiError}
 */
export function toApiError(status, body) {
  const fallback = STATUS_FALLBACKS[status] ?? `Request failed (${status}).`;

  if (typeof body === 'string' && body.trim()) {
    // A reverse proxy in front of this server answers 502s with an HTML page.
    // Dumping its markup into a notification helps nobody.
    const text = body.trim();
    return new ApiError(text.startsWith('<') ? fallback : text, {
      status,
      payload: body,
    });
  }

  const detail = body && typeof body === 'object' ? body.detail : null;

  return new ApiError(typeof detail === 'string' && detail.trim() ? detail : fallback, {
    status,
    payload: body,
  });
}
