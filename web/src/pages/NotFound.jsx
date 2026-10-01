import { Button, Stack, Text } from '@mantine/core';
import { Link } from 'react-router-dom';

import { Page } from '../layout/AppLayout.jsx';

export function NotFound() {
  return (
    <Page title="Not found">
      <Stack align="flex-start" gap="sm">
        <Text size="sm" c="dimmed">
          That page does not exist.
        </Text>
        <Button component={Link} to="/channels" variant="default">
          Back to Channels
        </Button>
      </Stack>
    </Page>
  );
}
