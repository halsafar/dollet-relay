import { useEffect, useMemo, useRef, useState } from 'react';
import { useDebouncedValue } from '@mantine/hooks';
import {
  Alert,
  Checkbox,
  Group,
  Loader,
  Select,
  Text,
  TextInput,
  Tooltip,
  UnstyledButton,
} from '@mantine/core';
import {
  ArrowUpDown,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ChevronUp,
  ChevronsLeft,
  ChevronsRight,
  Filter,
  GripVertical,
  Search,
} from 'lucide-react';
import {
  flexRender,
  getCoreRowModel,
  getFilteredRowModel,
  getPaginationRowModel,
  getSortedRowModel,
  useReactTable,
} from '@tanstack/react-table';

import classes from './DataTable.module.css';

const PAGE_SIZES = [25, 50, 100, 250];

/** How many numbered page buttons to show before falling back to a window. */
const PAGE_BUTTONS = 5;

/**
 * Case-insensitive substring match, applied to every column filter.
 *
 * This mirrors the backend's `LIKE '%x%'` search deliberately: a client-side
 * filter that tokenised differently would mean the same query returns different
 * rows depending on whether the table is filtered locally or on the server.
 */
function includesText(row, columnId, value) {
  if (!value) return true;
  return String(row.getValue(columnId) ?? '')
    .toLowerCase()
    .includes(String(value).toLowerCase());
}

function SortIcon({ direction }) {
  const props = { size: 13, className: classes.sortIconActive };
  if (direction === 'asc') return <ChevronUp {...props} />;
  if (direction === 'desc') return <ChevronDown {...props} />;
  return <ArrowUpDown size={13} className={classes.sortIcon} />;
}

/** A window of page indices centred on the current page. */
function pageWindow(pageIndex, pageCount) {
  if (pageCount <= PAGE_BUTTONS) {
    return Array.from({ length: pageCount }, (_, index) => index);
  }
  const half = Math.floor(PAGE_BUTTONS / 2);
  const start = Math.min(Math.max(pageIndex - half, 0), pageCount - PAGE_BUTTONS);
  return Array.from({ length: PAGE_BUTTONS }, (_, index) => start + index);
}

/**
 * The dense table every list screen is built from.
 *
 * Two modes. By default it holds the whole dataset and sorts, filters and pages
 * it client-side, which is what the channel lineup wants. Supplying `rowCount`
 * switches it to server-driven: the server owns order and paging, and the table
 * reports what it wants through `onQueryChange`.
 *
 * @param {object} props
 * @param {unknown[]} props.data
 * @param {import('@tanstack/react-table').ColumnDef<any>[]} props.columns
 * @param {(row: any, index: number) => string} [props.getRowId]
 * @param {boolean} [props.loading]
 * @param {Error | null} [props.error]
 * @param {string} [props.emptyMessage]
 * @param {boolean} [props.enableSelection]
 * @param {(ids: string[]) => void} [props.onSelectionChange]
 * @param {(row: any) => import('react').ReactNode} [props.rowActions]
 * @param {import('react').ReactNode} [props.toolbar]
 * @param {number} [props.pageSize]
 * @param {string} [props.label] Accessible name for the table element.
 * @param {number} [props.rowCount] Total rows on the server. Supplying this
 *   switches the table to server-driven paging, sorting and search.
 * @param {(query: {pageIndex: number, pageSize: number, search: string,
 *   ordering?: string}) => void} [props.onQueryChange] Server-driven mode only.
 * @param {string} [props.searchPlaceholder] Server-driven mode only.
 * @param {unknown} [props.filterKey] Any value the caller filters by outside
 *   this component. Changing it returns to the first page, because page 4 of
 *   the previous filter is usually past the end of the new one.
 * @param {(ids: string[]) => void} [props.onVisibleOrderChange] Told the ids of
 *   the rows as the table currently shows them — filtered and sorted, every
 *   page — whenever that order changes. For a caller whose action depends on
 *   the order the operator has put the table in.
 * @param {(ids: string[]) => void} [props.onReorder] Lets the operator drag
 *   rows, and is told the whole visible sequence after each move — not a
 *   from/to pair, because the caller owns the order and would only have to
 *   re-derive from it what the table already knows. Called while the mouse is
 *   still down, once per row the drag passes: the caller's data is the
 *   animation, so a caller that does not store what it is told shows a drag
 *   that does nothing until it ends. Client-side tables only: the order is the
 *   caller's own data, and a server-driven table is holding one page of an
 *   order that belongs to the server.
 * @param {(movedId: string, ids: string[]) => void} [props.onReorderEnd] The
 *   same order once the gesture is over — the mouse up, or the key press — and
 *   only when a row actually moved. For a caller that has something to write:
 *   `onReorder` fires per row the drag crosses, and committing each of those
 *   would be one request per row passed over.
 * @param {(row: any) => string} [props.rowLabel] Names a row for the reorder
 *   handle. Without it the handle is announced by row id, which tells a screen
 *   reader user nothing about what they are moving.
 */
export function DataTable({
  data,
  columns,
  getRowId,
  loading = false,
  error = null,
  emptyMessage = 'Nothing here yet.',
  enableSelection = false,
  onSelectionChange,
  rowActions,
  toolbar,
  pageSize = 50,
  label,
  rowCount,
  onQueryChange,
  searchPlaceholder = 'Search',
  filterKey,
  onVisibleOrderChange,
  onReorder,
  onReorderEnd,
  rowLabel,
}) {
  // Server-driven when the caller knows the total row count. The streams table
  // has to work this way: that endpoint always paginates because it can hold
  // tens of thousands of rows, so there is no complete client-side copy to
  // sort or filter.
  const serverSide = rowCount !== undefined;

  const [sorting, setSorting] = useState([]);
  const [columnFilters, setColumnFilters] = useState([]);
  const [rowSelection, setRowSelection] = useState({});
  const [showFilters, setShowFilters] = useState(true);
  const [pagination, setPagination] = useState({ pageIndex: 0, pageSize });
  const [search, setSearch] = useState('');
  // Typing must not fire a request per keystroke against a paginated endpoint.
  const [debouncedSearch] = useDebouncedValue(search, 250);

  const resolvedColumns = useMemo(() => {
    const list = [...columns];
    if (rowActions) {
      list.push({
        id: '__actions',
        header: '',
        enableSorting: false,
        enableColumnFilter: false,
        cell: ({ row }) => rowActions(row.original),
      });
    }
    return list;
  }, [columns, rowActions]);

  const rowIds = useMemo(() => {
    const ids = new Set();
    data.forEach((row, index) =>
      ids.add(getRowId ? getRowId(row, index) : String(index)),
    );
    return ids;
  }, [data, getRowId]);

  // A row that has gone away must not stay selected. These ids drive bulk
  // actions, so a stale one means deleting something the user cannot see.
  const selection = useMemo(() => {
    const kept = {};
    for (const [id, isSelected] of Object.entries(rowSelection)) {
      if (isSelected && rowIds.has(id)) kept[id] = true;
    }
    return kept;
  }, [rowSelection, rowIds]);

  // Prune the stored state too, not only the derived view. Deriving alone
  // would leave dropped ids in `rowSelection`, so paging away and back
  // resurrects a selection the user watched disappear.
  //
  // In server-driven mode `data` is one page, so this also means a selection
  // does not survive paging. That is the intended trade: a bulk delete should
  // only ever act on rows that were on screen when it was chosen.
  //
  // Adjusting state during render is React's documented pattern for reacting
  // to changed input; an effect here would re-render twice per data change.
  const stale = Object.entries(rowSelection).some(
    ([id, isSelected]) => !isSelected || !rowIds.has(id),
  );
  if (stale) setRowSelection(selection);

  // Returning to the first page when the query changes. TanStack's own reset is
  // disabled by `manualPagination`, and `filterKey` covers filters that live
  // outside this component entirely.
  const [lastQueryKey, setLastQueryKey] = useState({ sorting, filterKey });
  if (lastQueryKey.sorting !== sorting || lastQueryKey.filterKey !== filterKey) {
    setLastQueryKey({ sorting, filterKey });
    setPagination((current) =>
      current.pageIndex === 0 ? current : { ...current, pageIndex: 0 },
    );
  }

  // TanStack Table hands back a mutable instance rather than a value, which the
  // React Compiler rules flag as unmemoizable. That is inherent to the library
  // and harmless here: every piece of state it reads is React state above.
  // eslint-disable-next-line react-hooks/incompatible-library
  const table = useReactTable({
    data,
    columns: resolvedColumns,
    state: { sorting, columnFilters, rowSelection: selection, pagination },
    getRowId,
    enableRowSelection: enableSelection,
    onSortingChange: setSorting,
    onColumnFiltersChange: setColumnFilters,
    onRowSelectionChange: setRowSelection,
    onPaginationChange: setPagination,
    manualPagination: serverSide,
    manualSorting: serverSide,
    manualFiltering: serverSide,
    rowCount,
    filterFns: { includesText },
    // TanStack sorts numeric columns descending on the first click. Channel
    // numbers and stream counts both want the opposite, and a table where the
    // direction depends on the column's data type just reads as a bug.
    defaultColumn: { filterFn: includesText, sortDescFirst: false },
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getFilteredRowModel: getFilteredRowModel(),
    getPaginationRowModel: getPaginationRowModel(),
  });

  const selectedIds = useMemo(() => Object.keys(selection), [selection]);
  useEffect(() => {
    onSelectionChange?.(selectedIds);
  }, [selectedIds, onSelectionChange]);

  // The sorted-and-filtered model, before paging: what the operator sees is
  // the order across every page, not the one on screen.
  useEffect(() => {
    if (!onVisibleOrderChange) return;
    onVisibleOrderChange(table.getSortedRowModel().rows.map((row) => row.id));
    // `table` is stable; the model is derived from these.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [data, sorting, columnFilters, search, onVisibleOrderChange]);

  // The row in hand. A ref, because dragstart must not re-render: the picture
  // the pointer carries is taken from the source element after that handler
  // returns. `dragging` is the same id in state, set once the drag is over
  // some other row and it is safe to style the source.
  const dragged = useRef(null);
  const [dragging, setDragging] = useState(null);
  const body = useRef(null);
  const refocus = useRef(null);

  const visibleIds = () => table.getSortedRowModel().rows.map((row) => row.id);

  const moveRow = (fromId, toId) => {
    const ids = visibleIds();
    const from = ids.indexOf(fromId);
    const to = ids.indexOf(toId);
    // A reload landing mid-drag can take either row away. `splice(-1, 1)` cuts
    // the last row out instead of the missing one, so this is the difference
    // between a drag that does nothing and a lineup quietly rearranged.
    if (from === -1 || to === -1) return null;
    ids.splice(to, 0, ids.splice(from, 1)[0]);
    // A column sort outranks the caller's data order, so it would swallow the
    // move whole. Dragging takes over from it instead: the sequence reported
    // is the one that was on screen, with the row moved, so nothing else
    // shifts under the drop. Only when there is one to let go of — a sort
    // change returns to the first page, and a drag on page 4 must stay there.
    if (sorting.length > 0) setSorting([]);
    onReorder(ids);
    return ids;
  };

  const nudgeRow = (id, step) => {
    const ids = visibleIds();
    const to = ids.indexOf(id) + step;
    if (to < 0 || to >= ids.length) return;
    refocus.current = id;
    // One key press is a whole gesture, so it both moves and settles.
    const moved = moveRow(id, ids[to]);
    if (moved) onReorderEnd?.(id, moved);
  };

  // Moving a row moves its DOM node, and a moved node loses focus. Someone
  // holding Arrow Down would otherwise be re-finding the handle after every
  // single step.
  useEffect(() => {
    const id = refocus.current;
    if (id === null) return;
    refocus.current = null;
    for (const handle of body.current.querySelectorAll('[data-grip]')) {
      if (handle.dataset.grip === id) {
        handle.focus();
        break;
      }
    }
  });

  const { pageIndex: page, pageSize: size } = pagination;
  // The last query reported, so an unchanged one is not reported again: this
  // fires on mount with the values the caller already holds, and a caller
  // storing a new object with the same values fetches the same page twice.
  const reported = useRef(null);
  useEffect(() => {
    if (!serverSide) return;
    // `ordering` is the server's vocabulary: a field name with an optional `-`.
    const [sort] = sorting;
    const query = {
      pageIndex: page,
      pageSize: size,
      search: debouncedSearch,
      ordering: sort ? `${sort.desc ? '-' : ''}${sort.id}` : undefined,
    };

    const last = reported.current;
    if (
      last &&
      last.pageIndex === query.pageIndex &&
      last.pageSize === query.pageSize &&
      last.search === query.search &&
      last.ordering === query.ordering
    ) {
      return;
    }
    reported.current = query;
    onQueryChange?.(query);
  }, [serverSide, page, size, debouncedSearch, sorting, onQueryChange]);

  const filteredCount = serverSide ? rowCount : table.getFilteredRowModel().rows.length;
  const { pageIndex, pageSize: currentPageSize } = table.getState().pagination;
  const pageCount = table.getPageCount();
  const firstRow = filteredCount === 0 ? 0 : pageIndex * currentPageSize + 1;
  const lastRow = Math.min(
    pageIndex * currentPageSize + (serverSide ? data.length : currentPageSize),
    filteredCount,
  );
  const pageRows = table.getRowModel().rows;
  const columnCount =
    resolvedColumns.length + (enableSelection ? 1 : 0) + (onReorder ? 1 : 0);
  const headers = table.getHeaderGroups()[0].headers;

  // A caller's page size that is not one of the presets would otherwise render
  // the Select blank, which reads as "no page size set".
  const pageSizeOptions = useMemo(
    () =>
      [...new Set([...PAGE_SIZES, currentPageSize])]
        .sort((a, b) => a - b)
        .map((size) => String(size)),
    [currentPageSize],
  );

  return (
    <div className={classes.wrapper}>
      <div className={classes.toolbar}>
        {serverSide ? (
          // One box, because `search` is the only text filter this server
          // offers on a paginated list. Per-column boxes here would imply a
          // precision the query does not have.
          <TextInput
            size="xs"
            value={search}
            onChange={(event) => {
              setSearch(event.currentTarget.value);
              // Staying on page 5 of a result that now has one page shows
              // nothing and looks like a failed search.
              setPagination((current) => ({ ...current, pageIndex: 0 }));
            }}
            placeholder={searchPlaceholder}
            aria-label={searchPlaceholder}
            leftSection={<Search size={13} />}
            w={200}
          />
        ) : (
          <Tooltip label={showFilters ? 'Hide column filters' : 'Show column filters'}>
            <UnstyledButton
              onClick={() => setShowFilters((shown) => !shown)}
              aria-label="Toggle column filters"
              aria-pressed={showFilters}
              className={classes.toolbarButton}
            >
              <Filter
                size={15}
                color={
                  showFilters
                    ? 'var(--mantine-color-accent-5)'
                    : 'var(--mantine-color-dark-2)'
                }
              />
            </UnstyledButton>
          </Tooltip>
        )}
        {loading && <Loader size={14} color="gray" />}
        {toolbar && <div className={classes.toolbarActions}>{toolbar}</div>}
      </div>

      {error && (
        <Alert color="red" variant="light" radius={0} title="Could not load">
          {error.message}
        </Alert>
      )}

      <div className={classes.scroll}>
        <table className={classes.table} aria-label={label}>
          <thead>
            <tr>
              {onReorder && <th className={classes.gripCell} />}
              {enableSelection && (
                <th className={classes.selectCell}>
                  <Checkbox
                    aria-label="Select all rows"
                    checked={table.getIsAllPageRowsSelected()}
                    indeterminate={table.getIsSomePageRowsSelected()}
                    onChange={table.getToggleAllPageRowsSelectedHandler()}
                  />
                </th>
              )}
              {headers.map((header) => {
                const canSort = header.column.getCanSort();
                const canFilter =
                  !serverSide && showFilters && header.column.getCanFilter();
                const content = (
                  <>
                    {flexRender(header.column.columnDef.header, header.getContext())}
                    {canSort && <SortIcon direction={header.column.getIsSorted()} />}
                  </>
                );
                return (
                  <th key={header.id} style={{ width: header.column.columnDef.size }}>
                    {/* A real button rather than a div with role="button": Enter
                        and Space then work without a keydown handler. */}
                    {canSort ? (
                      <button
                        type="button"
                        className={`${classes.headCell} ${classes.sortable}`}
                        onClick={header.column.getToggleSortingHandler()}
                      >
                        {content}
                      </button>
                    ) : (
                      <div className={classes.headCell}>{content}</div>
                    )}
                    {/* Search lives in the column it searches,
                        rather than in one box that hides which field matched. */}
                    {canFilter && (
                      <div className={classes.headFilter}>
                        <TextInput
                          size="xs"
                          variant="filled"
                          placeholder="Search"
                          aria-label={`Search ${header.column.id}`}
                          leftSection={<Search size={12} />}
                          value={header.column.getFilterValue() ?? ''}
                          onChange={(event) =>
                            header.column.setFilterValue(event.currentTarget.value)
                          }
                        />
                      </div>
                    )}
                  </th>
                );
              })}
            </tr>
          </thead>
          <tbody ref={body}>
            {pageRows.map((row) => (
              <tr
                key={row.id}
                className={classes.row}
                data-selected={row.getIsSelected() || undefined}
                data-dragging={dragging === row.id || undefined}
                onDragOver={
                  onReorder
                    ? (event) => {
                        const carried = dragged.current;
                        if (carried === null || carried.id === row.id) return;
                        // Without this the row is not a drop target at all and
                        // the browser shows the "no" cursor over every one.
                        event.preventDefault();
                        setDragging(carried.id);

                        // The rows move while the pointer is still down, which
                        // is the only feedback there is: the picture under the
                        // pointer is the handle alone, and a drop marker says
                        // where a row would land rather than showing it there.
                        //
                        // They move as the pointer passes the middle of a row
                        // rather than the moment it touches an edge, where a
                        // pixel of hand tremor puts it back on the row it just
                        // left and the two trade places until the hand moves.
                        const ids = visibleIds();
                        const box = event.currentTarget.getBoundingClientRect();
                        const middle = box.top + box.height / 2;
                        const below = ids.indexOf(row.id) > ids.indexOf(carried.id);
                        if (below ? event.clientY < middle : event.clientY > middle) {
                          return;
                        }
                        // The order as it stood when the drag ends is what the
                        // caller commits, and a re-render is too late to read
                        // it: this is that order, kept as it is made.
                        carried.order = moveRow(carried.id, row.id) ?? carried.order;
                      }
                    : undefined
                }
                onDrop={
                  onReorder
                    ? (event) => {
                        // The rows are already where they were dragged to; this
                        // only stops the browser handling the drop itself.
                        event.preventDefault();
                      }
                    : undefined
                }
              >
                {onReorder && (
                  <td className={classes.gripCell}>
                    <UnstyledButton
                      draggable
                      data-grip={row.id}
                      className={classes.grip}
                      aria-label={`Move ${rowLabel ? rowLabel(row.original) : `row ${row.id}`}`}
                      onDragStart={(event) => {
                        dragged.current = { id: row.id, order: null };
                        // Firefox starts no drag at all from a source that has
                        // put nothing on the clipboard.
                        event.dataTransfer?.setData('text/plain', row.id);
                      }}
                      onDragEnd={() => {
                        const carried = dragged.current;
                        dragged.current = null;
                        setDragging(null);
                        if (carried?.order) onReorderEnd?.(carried.id, carried.order);
                      }}
                      // The handle is a button so that it is reachable at all;
                      // these make it usable once reached, since a native drag
                      // has no keyboard equivalent.
                      onKeyDown={(event) => {
                        const step =
                          event.key === 'ArrowUp'
                            ? -1
                            : event.key === 'ArrowDown'
                              ? 1
                              : 0;
                        if (step === 0) return;
                        event.preventDefault();
                        nudgeRow(row.id, step);
                      }}
                    >
                      <GripVertical size={13} />
                    </UnstyledButton>
                  </td>
                )}
                {enableSelection && (
                  <td className={classes.selectCell}>
                    <Checkbox
                      aria-label={`Select row ${row.id}`}
                      checked={row.getIsSelected()}
                      disabled={!row.getCanSelect()}
                      onChange={row.getToggleSelectedHandler()}
                    />
                  </td>
                )}
                {row.getVisibleCells().map((cell) => (
                  <td
                    key={cell.id}
                    className={
                      cell.column.id === '__actions' ? classes.actionCell : undefined
                    }
                  >
                    {flexRender(cell.column.columnDef.cell, cell.getContext())}
                  </td>
                ))}
              </tr>
            ))}
            {pageRows.length === 0 && (
              <tr>
                <td className={classes.empty} colSpan={columnCount}>
                  {loading ? 'Loading…' : emptyMessage}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      <div className={classes.footer}>
        <Group gap={6} wrap="nowrap">
          <Text size="xs" c="dimmed">
            Page size
          </Text>
          <Select
            data={pageSizeOptions}
            value={String(currentPageSize)}
            onChange={(value) => value && table.setPageSize(Number(value))}
            aria-label="Page size"
            w={78}
            allowDeselect={false}
            comboboxProps={{ withinPortal: true }}
          />
        </Group>
        <span className={classes.range}>
          {firstRow} to {lastRow} of {filteredCount}
        </span>
        <div className={classes.pager}>
          <PagerButton
            label="First page"
            icon={ChevronsLeft}
            disabled={!table.getCanPreviousPage()}
            onClick={() => table.setPageIndex(0)}
          />
          <PagerButton
            label="Previous page"
            icon={ChevronLeft}
            disabled={!table.getCanPreviousPage()}
            onClick={() => table.previousPage()}
          />
          {pageWindow(pageIndex, pageCount).map((index) => (
            <UnstyledButton
              key={index}
              className={classes.pageNumber}
              data-active={index === pageIndex || undefined}
              aria-label={`Page ${index + 1}`}
              aria-current={index === pageIndex ? 'page' : undefined}
              onClick={() => table.setPageIndex(index)}
            >
              {index + 1}
            </UnstyledButton>
          ))}
          <PagerButton
            label="Next page"
            icon={ChevronRight}
            disabled={!table.getCanNextPage()}
            onClick={() => table.nextPage()}
          />
          <PagerButton
            label="Last page"
            icon={ChevronsRight}
            disabled={!table.getCanNextPage()}
            onClick={() => table.setPageIndex(pageCount - 1)}
          />
        </div>
      </div>
    </div>
  );
}

function PagerButton({ label, icon: Icon, disabled, onClick }) {
  return (
    <UnstyledButton
      aria-label={label}
      disabled={disabled}
      onClick={onClick}
      className={classes.pagerButton}
      data-disabled={disabled || undefined}
    >
      <Icon size={15} />
    </UnstyledButton>
  );
}
