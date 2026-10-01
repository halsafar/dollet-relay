import { notifications } from '@mantine/notifications';

/**
 * The three things this application ever says.
 *
 * One place, so a page cannot title a refused source switch "Could not stop
 * it".
 */

/** Something worked. */
export function notifyDone(message) {
  notifications.show({ message, color: 'accent' });
}

/**
 * Something happened that the user asked for and does not need celebrating —
 * a delete, a cancel, a switch. Deliberately not red: a manual action
 * completing is not a warning.
 */
export function notifyQuiet(message) {
  notifications.show({ message, color: 'gray' });
}

/**
 * Something failed.
 *
 * `title` says which action, because the message is the server's and will not:
 * "a refresh is already running" reads very differently under "Delete failed".
 *
 * @param {string} title
 * @param {Error | {message?: string}} failure
 */
export function notifyError(title, failure) {
  notifications.show({
    title,
    message: failure?.message ?? 'Something went wrong.',
    color: 'red',
  });
}
