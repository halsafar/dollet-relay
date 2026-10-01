import { Component } from 'react';
import { Button, Code, Group, Stack, Text, Title } from '@mantine/core';
import { TriangleAlert } from 'lucide-react';

/**
 * Keeps one broken screen from blanking the whole app.
 *
 * The message and stack are shown rather than hidden behind "something went
 * wrong": this is a self-hosted tool whose only user is also the person who
 * would otherwise have to reproduce the bug to see it.
 */
export class ErrorBoundary extends Component {
  state = { error: null };

  static getDerivedStateFromError(error) {
    return { error };
  }

  componentDidCatch(error, info) {
    console.error('Unhandled error in render', error, info?.componentStack);
  }

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;

    return (
      <Stack p="xl" gap="sm" align="flex-start">
        <Group gap="xs">
          <TriangleAlert size={20} color="var(--mantine-color-red-5)" />
          <Title order={2}>This page crashed</Title>
        </Group>
        <Text size="sm" c="dimmed">
          {error.message || String(error)}
        </Text>
        {error.stack && (
          <Code block fz={11} style={{ maxHeight: 320, overflow: 'auto' }}>
            {error.stack}
          </Code>
        )}
        <Button variant="default" onClick={() => this.setState({ error: null })}>
          Try again
        </Button>
      </Stack>
    );
  }
}
