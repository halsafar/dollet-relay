import { useCallback, useMemo, useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import {
  Alert,
  Button,
  Group,
  Menu,
  Modal,
  NumberInput,
  Select,
  Stack,
  Switch,
  Table,
  Text,
  TextInput,
  Tooltip,
  UnstyledButton,
} from '@mantine/core';
import { useForm } from '@mantine/form';
import { EllipsisVertical, ListOrdered, Pencil, Plus, Trash2 } from 'lucide-react';

import { Page } from '../layout/AppLayout.jsx';
import { DataTable } from '../components/DataTable.jsx';
import { ConfirmModal } from '../components/ConfirmModal.jsx';
import { useResource } from '../api/useResource.js';
import { channelGroups, m3uAccounts, settings as settingsApi } from '../api/resources.js';
import { notifyDone, notifyError, notifyQuiet } from '../notify.js';
import { parseChannelNumber } from './channelNumber.js';
import classes from './Groups.module.css';
import { RowAction } from '../components/RowAction.jsx';

/*
 * The lineup's groups: how it is organised, and where its numbers come from.
 *
 * One row per group, not per provider link. A group's range is the group's —
 * two providers feeding "Canada" must not carry two of them — while whether a
 * provider's streams in the group are imported, and whether its new ones
 * become channels, is that provider's and shows in its own column.
 *
 * A provider with a hundred groups and a range on each is a table, not a
 * checkbox in a provider dialog.
 *
 * Rows drag, because Assign ranges hands out blocks in the order the table
 * shows and the guide's order is a taste — the name order the server sends is
 * rarely the order anyone wants their channels in. The arrangement is not
 * stored: it exists to be spent on the next Assign ranges, after which the
 * ranges themselves say where each group sits.
 */

/**
 * Stable on purpose: a new function per render re-derives the table's row ids
 * and its selection with them.
 */
const groupRowId = (group) => String(group.id);

const groupRowLabel = (group) => group.name;

/**
 * `groups` in the order the operator dragged them into: the ids they placed,
 * in that sequence, then anything they have not placed — a group a refresh has
 * just created — after those, in the order the server sent it.
 */
function arrange(groups, order) {
  if (order.length === 0) return groups;
  const rank = new Map(order.map((id, index) => [id, index]));
  const at = (group) => rank.get(groupRowId(group)) ?? Number.MAX_SAFE_INTEGER;
  return [...groups].sort((a, b) => at(a) - at(b));
}

const NUMBERING_KEY = 'numbering_settings';

/** The providers whose streams in the group keep the provider's own numbers. */
function providerNumberedBy(group, providers) {
  return group.links
    .filter((link) => link.enabled && link.numbering_mode === 'provider')
    .map(
      (link) =>
        providers.find((account) => account.id === link.m3u_account_id)?.name ??
        `account ${link.m3u_account_id}`,
    );
}

export function Groups() {
  const [params, setParams] = useSearchParams();
  const accountFilter = params.get('account');

  const loadGroups = useCallback(() => channelGroups.list(), []);
  const { data: groups, loading, error, reload } = useResource(loadGroups, []);
  const loadAccounts = useCallback(() => m3uAccounts.list(), []);
  const { data: accounts } = useResource(loadAccounts, []);

  const [editing, setEditing] = useState(null);
  const [confirming, setConfirming] = useState(null);
  const [selected, setSelected] = useState([]);
  // Row ids as the table shows them — the order Assign ranges hands out blocks in.
  const [visibleOrder, setVisibleOrder] = useState([]);
  // The order the operator dragged the rows into. Empty until they drag one,
  // and then it is what the table shows.
  const [order, setOrder] = useState([]);
  const [planning, setPlanning] = useState(null);
  const [renumbering, setRenumbering] = useState(null);

  const loadPolicy = useCallback(async () => {
    const groupsOfSettings = await settingsApi.list();
    return groupsOfSettings.find((entry) => entry.key === NUMBERING_KEY)?.value ?? null;
  }, []);
  const { data: policy, reload: reloadPolicy } = useResource(loadPolicy, null);

  // Only providers that have offered at least one group get a column; an
  // account with nothing imported yet has nothing to say here.
  const providers = useMemo(() => {
    const linked = new Set(
      groups.flatMap((group) => group.links.map((l) => l.m3u_account_id)),
    );
    return accounts.filter((account) => linked.has(account.id));
  }, [groups, accounts]);

  const visible = useMemo(
    () =>
      accountFilter
        ? groups.filter((group) =>
            group.links.some((link) => link.m3u_account_id === Number(accountFilter)),
          )
        : groups,
    [groups, accountFilter],
  );

  const arranged = useMemo(() => arrange(visible, order), [visible, order]);

  /**
   * The table reports the rows it is showing, which under a provider filter is
   * some of them. Re-sequencing only the places those rows hold leaves the
   * groups the filter hides where they were — herding them to the end would
   * rearrange a guide out of sight of the person rearranging it.
   */
  const reorder = useCallback(
    (ids) => {
      const moved = new Set(ids);
      let next = 0;
      setOrder(
        arrange(groups, order)
          .map(groupRowId)
          .map((id) => (moved.has(id) ? ids[next++] : id)),
      );
    },
    [groups, order],
  );

  const writeLink = useCallback(
    async (group, accountId, body) => {
      await m3uAccounts.setGroup(accountId, { channel_group: group.id, ...body });
      await reload();
    },
    [reload],
  );

  /**
   * A link write the server refused until acknowledged — disabling a group
   * costs its streams, switching auto-sync on creates channels — comes back
   * as a 409 naming the cost. Shown, then resent with `confirm`.
   */
  const saveLink = useCallback(
    async (group, accountId, body) => {
      try {
        await writeLink(group, accountId, body);
      } catch (failure) {
        if (failure.status === 409 && failure.payload?.cost) {
          setConfirming({
            kind: body.auto_channel_sync ? 'auto-sync' : 'disable',
            title: body.auto_channel_sync ? 'Turn on auto-sync' : 'Stop importing',
            confirmLabel: body.auto_channel_sync ? 'Turn on' : 'Stop importing',
            confirmColor: body.auto_channel_sync ? 'accent' : 'red',
            message: <Cost detail={failure.message} cost={failure.payload.cost} />,
            onConfirm: () =>
              writeLink(group, accountId, { ...body, confirm: true }).catch((refused) =>
                notifyError('Could not change the group', refused),
              ),
          });
          return;
        }
        notifyError('Could not change the group', failure);
      }
    },
    [writeLink],
  );

  const saveRange = useCallback(
    async (group, range) => {
      try {
        await channelGroups.update(group.id, {
          number_start: range.start,
          number_end: range.end,
        });
        await reload();
      } catch (failure) {
        notifyError('Could not save the range', failure);
      }
    },
    [reload],
  );

  const renumber = useCallback(
    async (group, order) => {
      try {
        const { renumbered } = await channelGroups.renumber(group.id, order);
        notifyDone(
          `Renumbered ${renumbered} ${renumbered === 1 ? 'channel' : 'channels'} in ${group.name}`,
        );
        await reload();
      } catch (failure) {
        notifyError('Could not renumber the group', failure);
      }
    },
    [reload],
  );

  const remove = useCallback(
    async (group) => {
      try {
        await channelGroups.remove(group.id);
        notifyQuiet(`Deleted ${group.name}`);
        await reload();
      } catch (failure) {
        notifyError('Could not delete the group', failure);
      }
    },
    [reload],
  );

  /**
   * The same two-pass gate `saveLink` uses, summed across the selection.
   *
   * Sending `confirm: true` on every row would delete five groups' streams
   * with no dialog — the outcome the server's 409 exists to prevent. The first
   * pass asks without `confirm` and collects the refusals; only if none come
   * back is the change already done.
   */
  const bulkImport = useCallback(
    async (accountId, enabled) => {
      const ids = selected.map(Number);
      const write = (id, confirm) =>
        m3uAccounts.setGroup(accountId, { channel_group: id, enabled, confirm });

      const attempts = await Promise.allSettled(ids.map((id) => write(id, false)));
      const refused = [];
      let failed = 0;
      attempts.forEach((attempt, index) => {
        if (attempt.status !== 'rejected') return;
        const failure = attempt.reason;
        if (failure?.status === 409 && failure.payload?.cost) {
          refused.push({ id: ids[index], cost: failure.payload.cost });
        } else {
          failed += 1;
        }
      });

      const done = async () => {
        if (failed > 0) {
          notifyError('Some groups were not changed', {
            message: `${failed} of ${ids.length} could not be updated.`,
          });
        } else {
          notifyDone(
            `${enabled ? 'Importing' : 'No longer importing'} ${ids.length} groups`,
          );
        }
        await reload();
      };

      if (refused.length === 0) {
        await done();
        return;
      }

      // One dialog for the whole selection rather than one per group: the
      // operator is deciding about the set, and the number they need is the
      // total.
      const total = refused.reduce(
        (sum, item) => ({
          streams_deleted: sum.streams_deleted + (item.cost.streams_deleted ?? 0),
          channels_left_unplayable:
            sum.channels_left_unplayable + (item.cost.channels_left_unplayable ?? 0),
          channels: [...sum.channels, ...(item.cost.channels ?? [])],
        }),
        { streams_deleted: 0, channels_left_unplayable: 0, channels: [] },
      );
      setConfirming({
        kind: 'disable',
        title: 'Stop importing',
        confirmLabel: 'Stop importing',
        confirmColor: 'red',
        message: (
          <Cost
            detail={`${refused.length} of ${ids.length} groups have streams that will be deleted, and the channel assignments pointing at them do not come back if the group is re-enabled.`}
            cost={total}
          />
        ),
        onConfirm: async () => {
          const second = await Promise.allSettled(
            refused.map((item) => write(item.id, true)),
          );
          failed += second.filter((attempt) => attempt.status === 'rejected').length;
          await done();
        },
      });
    },
    [selected, reload],
  );

  const savePolicy = useCallback(
    async (changes) => {
      try {
        await settingsApi.update(NUMBERING_KEY, changes);
        await reloadPolicy();
      } catch (failure) {
        // The server names the field; a step of zero should read as a
        // message under the step, not as "request rejected".
        const fields = failure.payload?.fields;
        notifyError('Could not save the numbering settings', {
          message: fields ? Object.values(fields).join('; ') : failure.message,
        });
      }
    },
    [reloadPolicy],
  );

  const openRangesPlan = useCallback(async () => {
    try {
      const plan = await channelGroups.planRanges(visibleOrder.map(Number));
      setPlanning({ kind: 'ranges', plan });
    } catch (failure) {
      notifyError('Could not plan the ranges', failure);
    }
  }, [visibleOrder]);

  const applyRangesPlan = useCallback(
    async (plan) => {
      try {
        const { assigned } = await channelGroups.assignRanges(plan.ranges);
        notifyDone(
          `Assigned ranges to ${assigned} ${assigned === 1 ? 'group' : 'groups'}`,
        );
        setPlanning(null);
        await reload();
      } catch (failure) {
        notifyError('Could not assign the ranges', failure);
      }
    },
    [reload],
  );

  const openRenumberPlan = useCallback(async (order = 'current') => {
    try {
      const plan = await channelGroups.planRenumber(order);
      setPlanning({ kind: 'renumber', plan, order });
    } catch (failure) {
      notifyError('Could not plan the renumber', failure);
    }
  }, []);

  const applyRenumberPlan = useCallback(
    async (order) => {
      try {
        const { renumbered, groups: count } = await channelGroups.renumberAll(order);
        notifyDone(
          `Renumbered ${renumbered} ${renumbered === 1 ? 'channel' : 'channels'} in ${count} ${count === 1 ? 'group' : 'groups'}`,
        );
        setPlanning(null);
        await reload();
      } catch (failure) {
        notifyError('Could not renumber', failure);
      }
    },
    [reload],
  );

  const confirmDelete = (group) =>
    setConfirming({
      kind: 'delete',
      title: 'Delete group',
      confirmLabel: 'Delete',
      confirmColor: 'red',
      message:
        `Delete ${group.name}? Its ${group.channel_count} ${group.channel_count === 1 ? 'channel keeps' : 'channels keep'} their numbers and fall back to the default group. ` +
        'A provider that still sends the group recreates it at its next refresh.',
      onConfirm: () => remove(group),
    });

  const columns = useMemo(
    () => [
      {
        accessorKey: 'name',
        header: 'Group',
        size: 220,
        cell: ({ getValue }) => <span className={classes.name}>{getValue()}</span>,
      },
      {
        accessorKey: 'channel_count',
        header: 'Channels',
        size: 90,
        enableColumnFilter: false,
        cell: ({ getValue }) => <span className={classes.count}>{getValue()}</span>,
      },
      {
        accessorKey: 'stream_count',
        header: 'Streams',
        size: 90,
        enableColumnFilter: false,
        cell: ({ getValue }) => <span className={classes.count}>{getValue()}</span>,
      },
      {
        id: 'range',
        accessorFn: (row) => row.number_start,
        header: 'Number range',
        size: 200,
        enableColumnFilter: false,
        cell: ({ row }) => <RangeCell group={row.original} onCommit={saveRange} />,
      },
      ...providers.map((account) => ({
        id: `provider-${account.id}`,
        header: account.name,
        size: 230,
        enableSorting: false,
        enableColumnFilter: false,
        cell: ({ row }) => (
          <ProviderCell
            group={row.original}
            account={account}
            onImport={(enabled) => saveLink(row.original, account.id, { enabled })}
            onAutoSync={(on) =>
              saveLink(row.original, account.id, { auto_channel_sync: on })
            }
          />
        ),
      })),
    ],
    [providers, saveRange, saveLink],
  );

  const rowActions = useCallback(
    (group) => (
      <Group gap={2} wrap="nowrap" justify="flex-end">
        <Tooltip
          label={
            providerNumberedBy(group, providers).length > 0
              ? `Numbered by ${providerNumberedBy(group, providers).join(', ')}: its channels keep the provider's numbers`
              : group.channel_count === 0
                ? 'No channels in this group'
                : group.number_start == null
                  ? 'Set a range to renumber into'
                  : `Move the group's ${group.channel_count} ${group.channel_count === 1 ? 'channel' : 'channels'} into the range — asks first`
          }
        >
          <Button
            variant="subtle"
            size="compact-xs"
            color="gray"
            leftSection={<ListOrdered size={12} />}
            disabled={
              group.channel_count === 0 ||
              group.number_start == null ||
              providerNumberedBy(group, providers).length > 0
            }
            aria-label={`Renumber ${group.name}`}
            onClick={() => setRenumbering(group)}
          >
            Renumber
          </Button>
        </Tooltip>
        <RowAction
          label={`Edit ${group.name}`}
          tooltip="Rename, or set the range"
          onClick={() => setEditing(group)}
        >
          <Pencil size={14} />
        </RowAction>
        <RowAction
          label={`Delete ${group.name}`}
          tooltip="Delete"
          color="red"
          onClick={() => confirmDelete(group)}
        >
          <Trash2 size={14} />
        </RowAction>
      </Group>
    ),
    // The confirms read only their argument; the providers name who numbers a group.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [providers],
  );

  const filterOptions = providers.map((account) => ({
    value: String(account.id),
    label: account.name,
  }));

  return (
    <Page
      title="Groups"
      subtitle="Where the lineup's channels are numbered, and which providers feed each group."
      actions={
        <Button size="xs" leftSection={<Plus size={14} />} onClick={() => setEditing({})}>
          New group
        </Button>
      }
    >
      <NumberingPanel
        policy={policy}
        onSave={savePolicy}
        onPlanRanges={openRangesPlan}
        onPlanRenumber={openRenumberPlan}
      />

      <DataTable
        label="Groups"
        data={arranged}
        columns={columns}
        getRowId={groupRowId}
        rowLabel={groupRowLabel}
        onReorder={reorder}
        onVisibleOrderChange={setVisibleOrder}
        loading={loading}
        error={error}
        enableSelection
        onSelectionChange={setSelected}
        rowActions={rowActions}
        filterKey={accountFilter}
        emptyMessage={error ? 'Groups could not be loaded.' : 'No groups yet.'}
        toolbar={
          <Group gap={6} wrap="nowrap">
            {providers.length > 1 && (
              <Select
                size="xs"
                data={filterOptions}
                value={accountFilter}
                onChange={(value) => setParams(value ? { account: value } : {})}
                placeholder="All providers"
                aria-label="Filter by provider"
                clearable
                clearButtonProps={{ 'aria-label': 'Clear provider filter' }}
                w={170}
                comboboxProps={{ withinPortal: true }}
              />
            )}
            {selected.length > 0 && providers.length > 0 && (
              <>
                <Text size="xs" c="dimmed">
                  {selected.length} selected
                </Text>
                <Menu withinPortal position="bottom-end">
                  <Menu.Target>
                    <UnstyledButton
                      aria-label="Bulk actions"
                      style={{ display: 'flex', padding: 4 }}
                    >
                      <EllipsisVertical size={15} />
                    </UnstyledButton>
                  </Menu.Target>
                  <Menu.Dropdown>
                    {providers.flatMap((account) => [
                      <Menu.Item
                        key={`on-${account.id}`}
                        onClick={() => bulkImport(account.id, true)}
                      >
                        Import from {account.name}
                      </Menu.Item>,
                      <Menu.Item
                        key={`off-${account.id}`}
                        onClick={() => bulkImport(account.id, false)}
                      >
                        Stop importing from {account.name}
                      </Menu.Item>,
                    ])}
                  </Menu.Dropdown>
                </Menu>
              </>
            )}
          </Group>
        }
      />

      {editing && (
        <GroupModal
          group={editing.id ? editing : null}
          providers={providers}
          onClose={() => setEditing(null)}
          onSaved={reload}
        />
      )}
      {planning?.kind === 'ranges' && (
        <RangesPlanModal
          plan={planning.plan}
          onApply={() => applyRangesPlan(planning.plan)}
          onClose={() => setPlanning(null)}
        />
      )}
      {planning?.kind === 'renumber' && (
        <RenumberPlanModal
          plan={planning.plan}
          order={planning.order}
          onOrderChange={openRenumberPlan}
          onApply={() => applyRenumberPlan(planning.order)}
          onClose={() => setPlanning(null)}
        />
      )}
      {renumbering && (
        <RenumberGroupModal
          group={renumbering}
          onApply={(order) => renumber(renumbering, order)}
          onClose={() => setRenumbering(null)}
        />
      )}
      {confirming && (
        <ConfirmModal
          title={confirming.title}
          message={confirming.message}
          confirmLabel={confirming.confirmLabel}
          confirmColor={confirming.confirmColor}
          onConfirm={confirming.onConfirm}
          onClose={() => setConfirming(null)}
        />
      )}
    </Page>
  );
}

/**
 * The lineup's numbering policy, and the two actions that apply it to every
 * group at once. Both actions show their plan before writing anything: what
 * the operator sees is what the same server code then does.
 */
function NumberingPanel({ policy, onSave, onPlanRanges, onPlanRenumber }) {
  const [block, setBlock] = useState(policy?.group_block_size ?? '');
  const [step, setStep] = useState(policy?.channel_step ?? '');
  // The settings arrive after the first render; the drafts follow them once.
  const [seen, setSeen] = useState(policy);
  if (policy !== seen) {
    setSeen(policy);
    setBlock(policy?.group_block_size ?? '');
    setStep(policy?.channel_step ?? '');
  }

  const commit = () => {
    if (!policy) return;
    const next = {
      group_block_size: parseChannelNumber(block) ?? policy.group_block_size,
      channel_step: parseChannelNumber(step) ?? policy.channel_step,
    };
    if (
      next.group_block_size === policy.group_block_size &&
      next.channel_step === policy.channel_step
    ) {
      return;
    }
    onSave(next);
  };

  return (
    <Group gap={10} wrap="nowrap" className={classes.policy}>
      <Text size="xs" c="dimmed">
        Numbering
      </Text>
      <NumberInput
        size="xs"
        w={96}
        hideControls
        min={1}
        label="Block size"
        aria-label="Group block size"
        value={block}
        onChange={setBlock}
        onBlur={commit}
        disabled={!policy}
      />
      <NumberInput
        size="xs"
        w={80}
        hideControls
        min={1}
        label="Channel step"
        aria-label="Channel step"
        value={step}
        onChange={setStep}
        onBlur={commit}
        disabled={!policy}
      />
      <Tooltip label="Give every group without a range the next free block, in the order the table shows — drag a row by its handle to change it">
        <Button
          size="xs"
          variant="default"
          onClick={onPlanRanges}
          className={classes.policyAction}
        >
          Assign ranges…
        </Button>
      </Tooltip>
      <Tooltip label="Lay every ranged group out on its range — shows the plan first">
        <Button
          size="xs"
          variant="default"
          onClick={() => onPlanRenumber()}
          className={classes.policyAction}
        >
          Renumber all…
        </Button>
      </Tooltip>
    </Group>
  );
}

function RangesPlanModal({ plan, onApply, onClose }) {
  const empty = plan.ranges.length === 0;
  return (
    <Modal opened onClose={onClose} title="Assign ranges" size="md">
      <Stack gap="sm">
        {empty ? (
          <Text size="sm" c="dimmed">
            Every group already has a range. Nothing to assign.
          </Text>
        ) : (
          <>
            <Text size="sm" c="dimmed">
              Blocks of {plan.block_size}, in the table&apos;s order, after everything
              already in use. Groups that have a range keep it.
            </Text>
            <Table fz="sm" verticalSpacing={4}>
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>Group</Table.Th>
                  <Table.Th>From</Table.Th>
                  <Table.Th>To</Table.Th>
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {plan.ranges.map((range) => (
                  <Table.Tr key={range.id}>
                    <Table.Td>{range.name}</Table.Td>
                    <Table.Td>{range.number_start}</Table.Td>
                    <Table.Td>{range.number_end}</Table.Td>
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </Table>
          </>
        )}
        <Group justify="flex-end" gap="xs">
          <Button variant="default" onClick={onClose}>
            {empty ? 'Close' : 'Cancel'}
          </Button>
          {!empty && <Button onClick={onApply}>Apply</Button>}
        </Group>
      </Stack>
    </Modal>
  );
}

/**
 * What a renumber walks the group in.
 *
 * A renumber compacts the lineup onto the grid; for an imported lineup the
 * current order is click order, so the key is a choice. Digits sort as
 * numbers, so `TSN2` comes before `TSN10`.
 */
const RENUMBER_ORDERS = [
  { value: 'current', label: 'The order they are in now' },
  { value: 'name', label: 'Channel name' },
  { value: 'guide', label: 'Guide name' },
  { value: 'tvg_id', label: 'tvg-id' },
];

const ORDER_NOTE =
  'Channels the key says nothing about — no guide match, no tvg-id — keep the order they have now, and go last.';

function OrderSelect({ value, onChange }) {
  return (
    <Select
      size="xs"
      label="Order by"
      description={value === 'current' ? undefined : ORDER_NOTE}
      data={RENUMBER_ORDERS}
      value={value}
      onChange={onChange}
      allowDeselect={false}
      comboboxProps={{ withinPortal: true }}
    />
  );
}

function RenumberPlanModal({ plan, order, onOrderChange, onApply, onClose }) {
  const total = plan.groups.reduce((sum, group) => sum + group.channels.length, 0);
  const empty = plan.groups.length === 0;
  return (
    <Modal opened onClose={onClose} title="Renumber all" size="md">
      <Stack gap="sm">
        <OrderSelect value={order} onChange={onOrderChange} />
        {empty ? (
          <Text size="sm" c="dimmed">
            No group can be renumbered: each needs a range and at least one channel.
          </Text>
        ) : (
          <>
            <Table fz="sm" verticalSpacing={4}>
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>Group</Table.Th>
                  <Table.Th>Range</Table.Th>
                  <Table.Th>Channels</Table.Th>
                  <Table.Th>Numbers</Table.Th>
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {plan.groups.map((group) => (
                  <Table.Tr key={group.id}>
                    <Table.Td>{group.name}</Table.Td>
                    <Table.Td>
                      {group.number_start}
                      {group.number_end == null ? ' up' : `–${group.number_end}`}
                    </Table.Td>
                    <Table.Td>{group.channels.length}</Table.Td>
                    <Table.Td>
                      {group.channels.length > 0 &&
                        `${group.channels[0].to} → ${group.channels[group.channels.length - 1].to}`}
                    </Table.Td>
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </Table>
            <Alert color="yellow" variant="light" p="xs" fz="xs">
              {total}{' '}
              {total === 1 ? 'channel takes a new number' : 'channels take new numbers'}.
              Plex identifies channels by number: re-scan the tuner&apos;s lineup
              afterwards.
            </Alert>
          </>
        )}
        {plan.skipped.length > 0 && (
          <Text size="xs" c="dimmed">
            Left alone:{' '}
            {plan.skipped.map((entry) => `${entry.name} (${entry.reason})`).join(', ')}.
          </Text>
        )}
        <Group justify="flex-end" gap="xs">
          <Button variant="default" onClick={onClose}>
            {empty ? 'Close' : 'Cancel'}
          </Button>
          {!empty && <Button onClick={onApply}>Apply</Button>}
        </Group>
      </Stack>
    </Modal>
  );
}

/**
 * One group's renumber. No per-channel preview: the numbers are just the range
 * laid out on the step, and what the operator is really choosing is the order.
 */
function RenumberGroupModal({ group, onApply, onClose }) {
  const [order, setOrder] = useState('current');
  const count = `${group.channel_count} ${group.channel_count === 1 ? 'channel' : 'channels'}`;
  return (
    <Modal opened onClose={onClose} title={`Renumber ${group.name}`} size="sm">
      <Stack gap="sm">
        <Text size="sm">
          {count} in {group.name} take new numbers, from {group.number_start}
          {group.number_end == null ? ' up' : ` to ${group.number_end}`}. Nothing outside
          the group moves.
        </Text>
        <OrderSelect value={order} onChange={setOrder} />
        <Alert color="yellow" variant="light" p="xs" fz="xs">
          Plex identifies channels by number: re-scan the tuner&apos;s lineup afterwards.
        </Alert>
        <Group justify="flex-end" gap="xs">
          <Button variant="default" onClick={onClose}>
            Cancel
          </Button>
          <Button
            onClick={() => {
              onApply(order);
              onClose();
            }}
          >
            Renumber
          </Button>
        </Group>
      </Stack>
    </Modal>
  );
}

/**
 * The range, editable in place and committed when a field is left: the server
 * checks the pair together, so sending each keystroke would refuse an end
 * typed before its start. Not re-synced from the row afterwards — a saved value
 * is the draft, and a refused one should stay on screen to be corrected.
 */
function RangeCell({ group, onCommit }) {
  const [start, setStart] = useState(group.number_start ?? '');
  const [end, setEnd] = useState(group.number_end ?? '');

  const commit = () => {
    const next = { start: parseChannelNumber(start), end: parseChannelNumber(end) };
    const stored = { start: group.number_start ?? null, end: group.number_end ?? null };
    if (next.start === stored.start && next.end === stored.end) return;
    onCommit(group, next);
  };

  return (
    <div className={classes.range}>
      <NumberInput
        size="xs"
        w={76}
        hideControls
        min={0}
        decimalScale={2}
        placeholder="from"
        aria-label={`${group.name} range start`}
        value={start}
        onChange={setStart}
        onBlur={commit}
      />
      <span className={classes.rangeDash}>–</span>
      <NumberInput
        size="xs"
        w={76}
        hideControls
        min={0}
        decimalScale={2}
        placeholder="to"
        aria-label={`${group.name} range end`}
        value={end}
        onChange={setEnd}
        onBlur={commit}
      />
    </div>
  );
}

/** One provider's standing with the group: imported, and auto-synced. */
function ProviderCell({ group, account, onImport, onAutoSync }) {
  const link = group.links.find((l) => l.m3u_account_id === account.id);
  if (!link) {
    return (
      <Text size="xs" c="dimmed">
        Not offered
      </Text>
    );
  }
  return (
    <div className={classes.provider}>
      <Switch
        size="xs"
        label="Import"
        labelPosition="left"
        classNames={{ label: classes.switchLabel }}
        checked={link.enabled}
        aria-label={`Import ${group.name} from ${account.name}`}
        onChange={(event) => onImport(event.currentTarget.checked)}
      />
      <Tooltip label="New streams in this group become channels at refresh, numbered in the range">
        <Switch
          size="xs"
          label="Auto-sync"
          labelPosition="left"
          classNames={{ label: classes.switchLabel }}
          checked={link.auto_channel_sync}
          disabled={!link.enabled}
          aria-label={`Auto-sync ${group.name} from ${account.name}`}
          onChange={(event) => onAutoSync(event.currentTarget.checked)}
        />
      </Tooltip>
    </div>
  );
}

/** The server's refusal, with whatever it enumerated. */
function Cost({ detail, cost }) {
  const channels = cost.channels ?? [];
  return (
    <Stack gap={6}>
      <span>{detail}</span>
      {channels.length > 0 && (
        <ul className={classes.costList}>
          {channels.map((channel) => (
            <li key={channel.stream}>
              {channel.name}
              <span className={classes.costNumber}>#{channel.channel_number}</span>
            </li>
          ))}
        </ul>
      )}
      {cost.streams_deleted != null && (
        <span>
          {cost.streams_deleted} streams deleted; {cost.channels_left_unplayable} channels
          left with nothing to play.
        </span>
      )}
    </Stack>
  );
}

const NUMBERING_OPTIONS = [
  { value: 'range', label: "The group's range" },
  { value: 'provider', label: "The provider's own numbers" },
];

/**
 * Create, or rename and re-range. The range is optional in both. Editing also
 * decides, per provider, whether that provider's channels take numbers from
 * the range or keep the ones the provider sends — an OTA tuner's `5.1` is the
 * channel's identity, and a range must not be allowed near it.
 */
function GroupModal({ group, providers, onClose, onSaved }) {
  const [busy, setBusy] = useState(false);
  const links = group?.links ?? [];
  const form = useForm({
    initialValues: {
      name: group?.name ?? '',
      number_start: group?.number_start ?? '',
      number_end: group?.number_end ?? '',
      numbering: Object.fromEntries(
        links.map((link) => [String(link.m3u_account_id), link.numbering_mode]),
      ),
    },
    validate: {
      name: (value) => (value.trim() ? null : 'A name is required'),
      number_end: (value, values) => {
        const end = parseChannelNumber(value);
        const start = parseChannelNumber(values.number_start);
        return end !== null && start !== null && end < start
          ? 'Must be at or above the start'
          : null;
      },
    },
  });

  const submit = async (values) => {
    setBusy(true);
    const payload = {
      name: values.name.trim(),
      number_start: parseChannelNumber(values.number_start),
      number_end: parseChannelNumber(values.number_end),
    };
    try {
      if (group) await channelGroups.update(group.id, payload);
      else await channelGroups.create(payload);
      for (const link of links) {
        const mode = values.numbering[String(link.m3u_account_id)];
        if (mode && mode !== link.numbering_mode) {
          await m3uAccounts.setGroup(link.m3u_account_id, {
            channel_group: group.id,
            numbering_mode: mode,
          });
        }
      }
      notifyDone(group ? `Saved ${payload.name}` : `Created ${payload.name}`);
      await onSaved();
      onClose();
    } catch (failure) {
      notifyError(
        group ? 'Could not save the group' : 'Could not create the group',
        failure,
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      opened
      onClose={onClose}
      title={group ? `Edit ${group.name}` : 'New group'}
      size="sm"
    >
      <form onSubmit={form.onSubmit(submit)}>
        <Stack gap="sm">
          <TextInput
            label="Name"
            withAsterisk
            data-autofocus
            {...form.getInputProps('name')}
          />
          <Group grow align="flex-start">
            <NumberInput
              label="Numbers from"
              description="Channels in this group take the next free number here."
              placeholder="None"
              min={0}
              decimalScale={2}
              hideControls
              {...form.getInputProps('number_start')}
            />
            <NumberInput
              label="Numbers to"
              description="Leave blank for no upper bound."
              placeholder="None"
              min={0}
              decimalScale={2}
              hideControls
              {...form.getInputProps('number_end')}
            />
          </Group>
          {links.map((link) => {
            const name =
              providers.find((account) => account.id === link.m3u_account_id)?.name ??
              `account ${link.m3u_account_id}`;
            return (
              <Select
                key={link.m3u_account_id}
                label={`Numbers from ${name}`}
                aria-label={`Numbers from ${name}`}
                data={NUMBERING_OPTIONS}
                allowDeselect={false}
                comboboxProps={{ withinPortal: true }}
                {...form.getInputProps(`numbering.${link.m3u_account_id}`)}
              />
            );
          })}
          <Group justify="flex-end" gap="xs">
            <Button variant="default" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" loading={busy}>
              {group ? 'Save' : 'Create'}
            </Button>
          </Group>
        </Stack>
      </form>
    </Modal>
  );
}
