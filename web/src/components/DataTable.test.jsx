import { describe, expect, it, vi } from 'vitest';
import { createEvent, fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { DataTable } from './DataTable.jsx';
import { renderWithProviders } from '../test-utils.jsx';
import { ApiError } from '../api/errors.js';

const COLUMNS = [
  { accessorKey: 'name', header: 'Name' },
  { accessorKey: 'group', header: 'Group' },
  { accessorKey: 'number', header: 'Number' },
];

const ROWS = [
  { id: 1, name: 'SNO One', group: 'UK', number: 101 },
  { id: 2, name: 'DRO HD', group: 'US', number: 4 },
  { id: 3, name: 'Alta', group: 'US', number: 7 },
];

const getRowId = (row) => String(row.id);

function setup(props = {}) {
  return renderWithProviders(
    <DataTable
      label="Channels"
      data={ROWS}
      columns={COLUMNS}
      getRowId={getRowId}
      {...props}
    />,
  );
}

/** Body rows only — the header and filter rows live in `thead`. */
function bodyRows() {
  const body = screen.getByRole('table').querySelector('tbody');
  return within(body).queryAllByRole('row');
}

function cellText(columnIndex) {
  return bodyRows().map((row) => row.querySelectorAll('td')[columnIndex].textContent);
}

describe('DataTable', () => {
  it('renders a row per record', () => {
    setup();
    expect(bodyRows()).toHaveLength(3);
    expect(screen.getByText('SNO One')).toBeInTheDocument();
  });

  it('shows the empty message when there is nothing to show', () => {
    setup({ data: [], emptyMessage: 'No channels.' });
    expect(screen.getByText('No channels.')).toBeInTheDocument();
  });

  it('says it is loading rather than showing the empty message', () => {
    setup({ data: [], loading: true, emptyMessage: 'No channels.' });
    expect(screen.getByText('Loading…')).toBeInTheDocument();
  });

  it('surfaces a load error above the rows', () => {
    setup({ error: new ApiError('Could not reach the server.', { status: 0 }) });
    expect(screen.getByText('Could not reach the server.')).toBeInTheDocument();
  });

  it('sorts ascending, then descending, then back to the source order', async () => {
    const user = userEvent.setup();
    setup();
    const header = screen.getByRole('button', { name: /Name/ });

    await user.click(header);
    expect(cellText(0)).toEqual(['Alta', 'DRO HD', 'SNO One']);

    await user.click(header);
    expect(cellText(0)).toEqual(['SNO One', 'DRO HD', 'Alta']);

    await user.click(header);
    expect(cellText(0)).toEqual(['SNO One', 'DRO HD', 'Alta']);
  });

  it('sorts from the keyboard', async () => {
    const user = userEvent.setup();
    setup();

    screen.getByRole('button', { name: /Number/ }).focus();
    await user.keyboard('{Enter}');

    expect(cellText(2)).toEqual(['4', '7', '101']);
  });

  it('puts a search box in each column header', () => {
    setup();

    for (const column of ['name', 'group', 'number']) {
      expect(
        screen.getByRole('textbox', { name: `Search ${column}` }),
      ).toBeInTheDocument();
    }
  });

  it('filters a single column without touching the others', async () => {
    const user = userEvent.setup();
    setup();

    await user.type(screen.getByRole('textbox', { name: 'Search group' }), 'uk');

    // "uk" appears in no name, so this only matches via the group column.
    expect(cellText(0)).toEqual(['SNO One']);
  });

  it('matches substrings the way the backend LIKE query does', async () => {
    const user = userEvent.setup();
    setup();

    await user.type(screen.getByRole('textbox', { name: 'Search name' }), 'HD');

    expect(cellText(0)).toEqual(['DRO HD']);
  });

  it('combines filters from two columns', async () => {
    const user = userEvent.setup();
    setup();

    await user.type(screen.getByRole('textbox', { name: 'Search group' }), 'us');
    expect(cellText(0)).toEqual(['DRO HD', 'Alta']);

    await user.type(screen.getByRole('textbox', { name: 'Search name' }), 'Alta');
    expect(cellText(0)).toEqual(['Alta']);
  });

  it('hides the column filters when the toggle is pressed', async () => {
    const user = userEvent.setup();
    setup();

    await user.click(screen.getByRole('button', { name: 'Toggle column filters' }));

    expect(
      screen.queryByRole('textbox', { name: 'Search name' }),
    ).not.toBeInTheDocument();
  });

  it('offers no drag handle to a caller that cannot take an order', () => {
    setup();
    expect(screen.queryByRole('button', { name: /^Move / })).not.toBeInTheDocument();
  });

  it('names the handle after the row it moves', () => {
    setup({ onReorder: vi.fn(), rowLabel: (row) => row.name });
    expect(screen.getByRole('button', { name: 'Move Alta' })).toBeInTheDocument();
  });

  it('falls back to the row id when the caller names nothing', () => {
    setup({ onReorder: vi.fn() });
    expect(screen.getByRole('button', { name: 'Move row 3' })).toBeInTheDocument();
  });

  it('reports the move while the mouse is still down', () => {
    const onReorder = vi.fn();
    setup({ onReorder, rowLabel: (row) => row.name });

    fireEvent.dragStart(screen.getByRole('button', { name: 'Move Alta' }));
    fireEvent.dragOver(bodyRows()[0]);

    // Nothing has been dropped yet: the rows move under the pointer, and the
    // drop only ends the drag.
    expect(onReorder).toHaveBeenCalledWith(['3', '1', '2']);
  });

  it('marks the row in hand, and lets go of it at the end of the drag', () => {
    const onReorder = vi.fn();
    setup({ onReorder, rowLabel: (row) => row.name });

    const handle = screen.getByRole('button', { name: 'Move SNO One' });
    const [first, middle] = bodyRows();
    fireEvent.dragStart(handle);
    fireEvent.dragOver(middle);
    expect(first).toHaveAttribute('data-dragging');

    fireEvent.dragEnd(handle);
    expect(first).not.toHaveAttribute('data-dragging');
  });

  it('moves a row only once the pointer is past the middle of its neighbour', () => {
    const onReorder = vi.fn();
    setup({ onReorder, rowLabel: (row) => row.name });

    const last = bodyRows()[2];
    vi.spyOn(last, 'getBoundingClientRect').mockReturnValue({
      top: 100,
      height: 20,
      bottom: 120,
      left: 0,
      right: 100,
      width: 100,
    });
    // jsdom has no DragEvent, and the plain Event it falls back to drops the
    // pointer position out of the init — so it goes on by hand.
    const dragOverAt = (row, clientY) => {
      const event = createEvent.dragOver(row);
      Object.defineProperty(event, 'clientY', { value: clientY });
      fireEvent(row, event);
    };

    // A pointer barely onto the row is a hand still deciding, not a move. At
    // an edge a tremor would otherwise trade the two rows back and forth.
    fireEvent.dragStart(screen.getByRole('button', { name: 'Move SNO One' }));
    dragOverAt(last, 104);
    expect(onReorder).not.toHaveBeenCalled();

    dragOverAt(last, 116);
    expect(onReorder).toHaveBeenCalledWith(['2', '3', '1']);
  });

  it('drops the drag when a reload takes the carried row away', () => {
    const onReorder = vi.fn();
    const props = {
      label: 'Channels',
      columns: COLUMNS,
      getRowId,
      onReorder,
      rowLabel: (row) => row.name,
    };
    const { rerender } = renderWithProviders(<DataTable {...props} data={ROWS} />);

    fireEvent.dragStart(screen.getByRole('button', { name: 'Move SNO One' }));
    rerender(<DataTable {...props} data={ROWS.filter((row) => row.id !== 1)} />);
    fireEvent.dragOver(bodyRows()[1]);

    // Reordering around a row that is no longer there would move whichever row
    // happens to be last, which is not what anyone dragged.
    expect(onReorder).not.toHaveBeenCalled();
  });

  it('takes nothing from a drag that is not a row moving', () => {
    const onReorder = vi.fn();
    setup({ onReorder, rowLabel: (row) => row.name });

    // Something else dragged over the table — a file, a selection — moves no
    // row and drops nothing.
    const [first, , last] = bodyRows();
    fireEvent.dragOver(last);
    fireEvent.drop(last);
    expect(onReorder).not.toHaveBeenCalled();

    // Nor has a row dragged over itself moved.
    fireEvent.dragStart(screen.getByRole('button', { name: 'Move SNO One' }));
    fireEvent.dragOver(first);
    expect(first).not.toHaveAttribute('data-dragging');
    expect(onReorder).not.toHaveBeenCalled();
  });

  it('ignores keys on the handle that are not the arrows', async () => {
    const user = userEvent.setup();
    const onReorder = vi.fn();
    setup({ onReorder, rowLabel: (row) => row.name });

    screen.getByRole('button', { name: 'Move DRO HD' }).focus();
    await user.keyboard('{Enter}');

    expect(onReorder).not.toHaveBeenCalled();
  });

  it('moves a row with the arrow keys, and not past either end', async () => {
    const user = userEvent.setup();
    const onReorder = vi.fn();
    setup({ onReorder, rowLabel: (row) => row.name });

    screen.getByRole('button', { name: 'Move DRO HD' }).focus();
    await user.keyboard('{ArrowUp}');
    expect(onReorder).toHaveBeenLastCalledWith(['2', '1', '3']);

    // The caller holds the order, so these rows have not actually moved: the
    // first one is still at the top and has nowhere above it to go.
    screen.getByRole('button', { name: 'Move SNO One' }).focus();
    await user.keyboard('{ArrowUp}');
    expect(onReorder).toHaveBeenCalledTimes(1);
  });

  it('lets a drag take over from a column sort', async () => {
    const user = userEvent.setup();
    const onReorder = vi.fn();
    setup({ onReorder, rowLabel: (row) => row.name });

    await user.click(screen.getByRole('button', { name: /Name/ }));
    // The handle takes the first cell, so the names have shifted one right.
    expect(cellText(1)).toEqual(['Alta', 'DRO HD', 'SNO One']);

    // The sequence reported is the sorted one with the row moved, rather than
    // a move into an order the sort was hiding.
    screen.getByRole('button', { name: 'Move SNO One' }).focus();
    await user.keyboard('{ArrowUp}');
    expect(onReorder).toHaveBeenCalledWith(['3', '1', '2']);
    // ...and the sort lets go, so the caller's order is what shows.
    expect(cellText(1)).toEqual(['SNO One', 'DRO HD', 'Alta']);
  });

  it('leaves an unsorted table on the page the row was moved on', async () => {
    const user = userEvent.setup();
    const onReorder = vi.fn();
    setup({ onReorder, rowLabel: (row) => row.name, pageSize: 2 });

    await user.click(screen.getByRole('button', { name: 'Next page' }));
    expect(screen.getByText('3 to 3 of 3')).toBeInTheDocument();

    screen.getByRole('button', { name: 'Move Alta' }).focus();
    await user.keyboard('{ArrowUp}');

    expect(onReorder).toHaveBeenCalledWith(['1', '3', '2']);
    expect(screen.getByText('3 to 3 of 3')).toBeInTheDocument();
  });

  it('reports selections by row id', async () => {
    const user = userEvent.setup();
    const onSelectionChange = vi.fn();
    setup({ enableSelection: true, onSelectionChange });

    await user.click(screen.getByRole('checkbox', { name: 'Select row 2' }));

    expect(onSelectionChange).toHaveBeenLastCalledWith(['2']);
  });

  it('selects and clears every row on the page from the header checkbox', async () => {
    const user = userEvent.setup();
    const onSelectionChange = vi.fn();
    setup({ enableSelection: true, onSelectionChange });

    const selectAll = screen.getByRole('checkbox', { name: 'Select all rows' });
    await user.click(selectAll);
    expect(onSelectionChange).toHaveBeenLastCalledWith(['1', '2', '3']);

    await user.click(selectAll);
    expect(onSelectionChange).toHaveBeenLastCalledWith([]);
  });

  it('shows the header checkbox as indeterminate for a partial selection', async () => {
    const user = userEvent.setup();
    setup({ enableSelection: true });

    await user.click(screen.getByRole('checkbox', { name: 'Select row 1' }));

    const selectAll = screen.getByRole('checkbox', { name: 'Select all rows' });
    expect(selectAll).toHaveProperty('indeterminate', true);
    expect(selectAll.checked).toBe(false);
  });

  it('only selects rows that survived the filter', async () => {
    const user = userEvent.setup();
    const onSelectionChange = vi.fn();
    setup({ enableSelection: true, onSelectionChange });

    await user.type(screen.getByRole('textbox', { name: 'Search group' }), 'US');
    await user.click(screen.getByRole('checkbox', { name: 'Select all rows' }));

    expect(onSelectionChange).toHaveBeenLastCalledWith(['2', '3']);
  });

  it('drops a selected row that is no longer in the data', async () => {
    const user = userEvent.setup();
    const onSelectionChange = vi.fn();
    const { rerender } = renderWithProviders(
      <DataTable
        label="Channels"
        data={ROWS}
        columns={COLUMNS}
        getRowId={getRowId}
        enableSelection
        onSelectionChange={onSelectionChange}
      />,
    );

    await user.click(screen.getByRole('checkbox', { name: 'Select row 2' }));
    expect(onSelectionChange).toHaveBeenLastCalledWith(['2']);

    // Row 2 was deleted elsewhere and the list reloaded. Keeping its id would
    // aim the next bulk action at a row that no longer exists.
    rerender(
      <DataTable
        label="Channels"
        data={ROWS.filter((row) => row.id !== 2)}
        columns={COLUMNS}
        getRowId={getRowId}
        enableSelection
        onSelectionChange={onSelectionChange}
      />,
    );

    expect(onSelectionChange).toHaveBeenLastCalledWith([]);
    expect(screen.getByRole('checkbox', { name: 'Select all rows' }).checked).toBe(false);
  });

  it('renders inline row actions and passes the original record through', async () => {
    const user = userEvent.setup();
    const onEdit = vi.fn();
    setup({
      rowActions: (row) => (
        <button type="button" onClick={() => onEdit(row)}>
          Edit {row.name}
        </button>
      ),
    });

    await user.click(screen.getByRole('button', { name: 'Edit DRO HD' }));

    expect(onEdit).toHaveBeenCalledWith(ROWS[1]);
  });

  it('paginates and reports the visible range', async () => {
    const user = userEvent.setup();
    setup({ pageSize: 2 });

    expect(bodyRows()).toHaveLength(2);
    expect(screen.getByText('1 to 2 of 3')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Next page' }));

    expect(cellText(0)).toEqual(['Alta']);
    expect(screen.getByText('3 to 3 of 3')).toBeInTheDocument();
  });

  it('offers a numbered button per page and marks the current one', async () => {
    const user = userEvent.setup();
    setup({ pageSize: 1 });

    expect(screen.getByRole('button', { name: 'Page 1' })).toHaveAttribute(
      'aria-current',
      'page',
    );

    await user.click(screen.getByRole('button', { name: 'Page 3' }));

    expect(cellText(0)).toEqual(['Alta']);
    expect(screen.getByRole('button', { name: 'Page 3' })).toHaveAttribute(
      'aria-current',
      'page',
    );
    expect(screen.getByRole('button', { name: 'Page 1' })).not.toHaveAttribute(
      'aria-current',
    );
  });

  it('windows the page buttons rather than rendering hundreds', () => {
    const many = Array.from({ length: 60 }, (_, index) => ({
      id: index,
      name: `Channel ${index}`,
      group: 'UK',
      number: index,
    }));
    setup({ data: many, pageSize: 1 });

    const numbered = screen
      .getAllByRole('button')
      .filter((button) => /^Page \d+$/.test(button.getAttribute('aria-label') ?? ''));

    expect(numbered).toHaveLength(5);
  });

  it('disables the pager at both ends', async () => {
    const user = userEvent.setup();
    setup({ pageSize: 2 });

    expect(screen.getByRole('button', { name: 'Previous page' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Next page' })).toBeEnabled();

    await user.click(screen.getByRole('button', { name: 'Last page' }));

    expect(screen.getByRole('button', { name: 'Next page' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'First page' })).toBeEnabled();
  });

  it('changes the page size', async () => {
    const user = userEvent.setup();
    setup({ pageSize: 2 });

    expect(bodyRows()).toHaveLength(2);

    await user.click(screen.getByRole('textbox', { name: 'Page size' }));
    // jsdom never lays the floating dropdown out, so it stays `display: none`
    // and the options are outside the accessible tree.
    await user.click(screen.getByRole('option', { name: '25', hidden: true }));

    expect(bodyRows()).toHaveLength(3);
    expect(screen.getByText('1 to 3 of 3')).toBeInTheDocument();
  });

  it('offers a caller page size that is not one of the presets', () => {
    setup({ pageSize: 2 });
    expect(screen.getByRole('textbox', { name: 'Page size' })).toHaveValue('2');
  });

  it('reports an empty range for a filter that matches nothing', async () => {
    const user = userEvent.setup();
    setup();

    await user.type(
      screen.getByRole('textbox', { name: 'Search name' }),
      'nothing matches',
    );

    expect(screen.getByText('0 to 0 of 0')).toBeInTheDocument();
  });

  it('renders caller-supplied toolbar content', () => {
    setup({ toolbar: <span>2 selected</span> });
    expect(screen.getByText('2 selected')).toBeInTheDocument();
  });
});

describe('DataTable in server-driven mode', () => {
  it('trusts the server row count over the rows on screen', () => {
    setup({ rowCount: 500, onQueryChange: vi.fn() });

    expect(screen.getByText('1 to 3 of 500')).toBeInTheDocument();
  });

  it('swaps per-column filters for the one search the server supports', () => {
    setup({ rowCount: 500, onQueryChange: vi.fn(), searchPlaceholder: 'Search streams' });

    expect(screen.getByRole('textbox', { name: 'Search streams' })).toBeInTheDocument();
    expect(
      screen.queryByRole('textbox', { name: 'Search name' }),
    ).not.toBeInTheDocument();
  });

  it('reports the page and size it wants', async () => {
    const user = userEvent.setup();
    const onQueryChange = vi.fn();
    setup({ rowCount: 500, onQueryChange, pageSize: 25 });

    await waitFor(() =>
      expect(onQueryChange).toHaveBeenCalledWith(
        expect.objectContaining({ pageIndex: 0, pageSize: 25 }),
      ),
    );

    await user.click(screen.getByRole('button', { name: 'Next page' }));

    await waitFor(() =>
      expect(onQueryChange).toHaveBeenLastCalledWith(
        expect.objectContaining({ pageIndex: 1 }),
      ),
    );
  });

  it('debounces the search and resets to the first page', async () => {
    const user = userEvent.setup();
    const onQueryChange = vi.fn();
    setup({ rowCount: 500, onQueryChange });

    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await waitFor(() =>
      expect(onQueryChange).toHaveBeenLastCalledWith(
        expect.objectContaining({ pageIndex: 1 }),
      ),
    );

    await user.type(screen.getByRole('textbox', { name: 'Search' }), 'vrix');

    await waitFor(() =>
      expect(onQueryChange).toHaveBeenLastCalledWith(
        // Back to page one: staying on page 2 of a one-page result shows
        // nothing and reads as a failed search.
        expect.objectContaining({ search: 'vrix', pageIndex: 0 }),
      ),
    );

    // One call for the settled value, not one per keystroke.
    const searches = onQueryChange.mock.calls.filter(
      ([query]) => query.search === 'vrix',
    );
    expect(searches.length).toBeLessThan(4);
  });

  it('translates sorting into the server ordering vocabulary', async () => {
    const user = userEvent.setup();
    const onQueryChange = vi.fn();
    setup({ rowCount: 500, onQueryChange });

    await user.click(screen.getByRole('button', { name: /Name/ }));
    await waitFor(() =>
      expect(onQueryChange).toHaveBeenLastCalledWith(
        expect.objectContaining({ ordering: 'name' }),
      ),
    );

    await user.click(screen.getByRole('button', { name: /Name/ }));
    await waitFor(() =>
      expect(onQueryChange).toHaveBeenLastCalledWith(
        expect.objectContaining({ ordering: '-name' }),
      ),
    );
  });

  it('does not sort or filter the rows itself', async () => {
    const user = userEvent.setup();
    setup({ rowCount: 500, onQueryChange: vi.fn() });

    await user.click(screen.getByRole('button', { name: /Name/ }));

    // The server owns the order; re-sorting the page locally would show a
    // different order than the one that produced these rows.
    expect(cellText(0)).toEqual(['SNO One', 'DRO HD', 'Alta']);
  });

  it('leaves client-side tables untouched', async () => {
    const user = userEvent.setup();
    const onQueryChange = vi.fn();
    setup({ onQueryChange });

    await user.click(screen.getByRole('button', { name: /Name/ }));

    expect(cellText(0)).toEqual(['Alta', 'DRO HD', 'SNO One']);
    expect(onQueryChange).not.toHaveBeenCalled();
  });
});

describe('DataTable state resets', () => {
  it('returns to the first page when the sort changes', async () => {
    const user = userEvent.setup();
    const onQueryChange = vi.fn();
    setup({ rowCount: 500, onQueryChange, pageSize: 1 });

    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await waitFor(() =>
      expect(onQueryChange).toHaveBeenLastCalledWith(
        expect.objectContaining({ pageIndex: 1 }),
      ),
    );

    await user.click(screen.getByRole('button', { name: /Name/ }));

    // manualPagination disables TanStack's own reset, so page 4 of the old
    // order would survive into a new one that may not have four pages.
    await waitFor(() =>
      expect(onQueryChange).toHaveBeenLastCalledWith(
        expect.objectContaining({ ordering: 'name', pageIndex: 0 }),
      ),
    );
  });

  it('returns to the first page when a filter outside the table changes', async () => {
    const user = userEvent.setup();
    const onQueryChange = vi.fn();
    const { rerender } = renderWithProviders(
      <DataTable
        label="Channels"
        data={ROWS}
        columns={COLUMNS}
        getRowId={getRowId}
        rowCount={500}
        onQueryChange={onQueryChange}
        filterKey={null}
      />,
    );

    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await waitFor(() =>
      expect(onQueryChange).toHaveBeenLastCalledWith(
        expect.objectContaining({ pageIndex: 1 }),
      ),
    );

    rerender(
      <DataTable
        label="Channels"
        data={ROWS}
        columns={COLUMNS}
        getRowId={getRowId}
        rowCount={5}
        onQueryChange={onQueryChange}
        filterKey="a-small-group"
      />,
    );

    // Otherwise: page 2 of a 5-row result, an empty table, and a footer
    // reading "51 to 5 of 5".
    await waitFor(() =>
      expect(onQueryChange).toHaveBeenLastCalledWith(
        expect.objectContaining({ pageIndex: 0 }),
      ),
    );
  });

  it('does not resurrect a selection after the rows come back', async () => {
    const user = userEvent.setup();
    const onSelectionChange = vi.fn();
    const props = {
      label: 'Channels',
      columns: COLUMNS,
      getRowId,
      enableSelection: true,
      onSelectionChange,
    };
    const { rerender } = renderWithProviders(<DataTable {...props} data={ROWS} />);

    await user.click(screen.getByRole('checkbox', { name: 'Select row 2' }));
    expect(onSelectionChange).toHaveBeenLastCalledWith(['2']);

    // Row 2 leaves — a page change, or a reload after someone else deleted it.
    rerender(<DataTable {...props} data={ROWS.filter((row) => row.id !== 2)} />);
    expect(onSelectionChange).toHaveBeenLastCalledWith([]);

    // ...and comes back. The user watched the selection clear; it must not
    // return and then feed a Delete they never asked for.
    rerender(<DataTable {...props} data={ROWS} />);
    expect(onSelectionChange).toHaveBeenLastCalledWith([]);
    expect(screen.getByRole('checkbox', { name: 'Select row 2' })).not.toBeChecked();
  });
});
