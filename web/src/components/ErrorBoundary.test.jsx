import { afterEach, describe, expect, it, vi } from 'vitest';
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { ErrorBoundary } from './ErrorBoundary.jsx';
import { renderWithProviders } from '../test-utils.jsx';

function Boom({ explode }) {
  if (explode) throw new Error('render exploded');
  return <p>all good</p>;
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe('ErrorBoundary', () => {
  it('renders its children when nothing throws', () => {
    renderWithProviders(
      <ErrorBoundary>
        <Boom explode={false} />
      </ErrorBoundary>,
    );
    expect(screen.getByText('all good')).toBeInTheDocument();
  });

  it('shows the message rather than a blank screen', () => {
    vi.spyOn(console, 'error').mockImplementation(() => {});

    renderWithProviders(
      <ErrorBoundary>
        <Boom explode />
      </ErrorBoundary>,
    );

    expect(screen.getByText('This page crashed')).toBeInTheDocument();
    expect(screen.getByText('render exploded')).toBeInTheDocument();
  });

  it('recovers when the cause is gone and Try again is pressed', async () => {
    vi.spyOn(console, 'error').mockImplementation(() => {});
    const user = userEvent.setup();

    const { rerender } = renderWithProviders(
      <ErrorBoundary>
        <Boom explode />
      </ErrorBoundary>,
    );

    rerender(
      <ErrorBoundary>
        <Boom explode={false} />
      </ErrorBoundary>,
    );
    await user.click(screen.getByRole('button', { name: 'Try again' }));

    expect(screen.getByText('all good')).toBeInTheDocument();
  });
});
