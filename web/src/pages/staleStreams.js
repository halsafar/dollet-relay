/**
 * What "delete stale streams after N days" will actually do.
 *
 * This one number decides whether a provider outage costs the user their
 * lineup, and a bare spinner labelled `stale_stream_days` says none of that.
 */
export function describeStaleDays(days) {
  if (!days || days <= 0) {
    return 'Nothing is deleted automatically. A stream the provider stops listing is kept and marked stale.';
  }
  return `A stream the provider stops listing is deleted after ${days} ${
    days === 1 ? 'day' : 'days'
  }. A provider outage lasting longer than that removes those streams, and any channel using one loses it.`;
}
