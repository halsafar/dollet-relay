import { Button, Group, Modal, Text } from '@mantine/core';

/**
 * Confirmation for an action that cannot be undone.
 *
 * Every destructive path in this app is a single click on an irreversible
 * server-side delete — there is no trash, no audit log and no undo, so the
 * confirmation is the only thing between a misclick and lost data. The same
 * gate fronts the two writes that are not deletes but still cannot be taken
 * back cheaply: renumbering a group, and switching auto-sync on, which creates
 * channels at the next refresh. Those pass `confirmColor` so the button does
 * not say "danger" about a create.
 *
 * `message` may be a node: the auto-sync preview is a list, and a list inside
 * a `<p>` is invalid, hence the `div`.
 */
export function ConfirmModal({
  title,
  message,
  confirmLabel = 'Delete',
  confirmColor = 'red',
  onConfirm,
  onClose,
}) {
  return (
    <Modal opened onClose={onClose} title={title} size="sm">
      <Text component="div" size="sm" c="dimmed" mb="md">
        {message}
      </Text>
      <Group justify="flex-end" gap="xs">
        <Button variant="default" onClick={onClose}>
          Cancel
        </Button>
        <Button
          color={confirmColor}
          onClick={() => {
            onConfirm();
            onClose();
          }}
        >
          {confirmLabel}
        </Button>
      </Group>
    </Modal>
  );
}
