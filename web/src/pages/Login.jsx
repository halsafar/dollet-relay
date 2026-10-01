import { useCallback, useState } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import {
  Alert,
  Box,
  Button,
  Center,
  Loader,
  Paper,
  PasswordInput,
  Stack,
  Text,
  TextInput,
} from '@mantine/core';
import { useForm } from '@mantine/form';
import { TriangleAlert } from 'lucide-react';

import { auth } from '../api/resources.js';
import { DolletMark } from '../components/DolletMark.jsx';
import { useResource } from '../api/useResource.js';
import { useSession } from '../auth/session.js';

/**
 * Sign in, or create the first administrator on an empty instance.
 *
 * One screen rather than two routes, because which one applies is a property
 * of the server rather than of where the user navigated. Without this a fresh
 * install has no way in at all: the login form would 401 forever against a
 * database with no accounts in it.
 */
export function Login() {
  const login = useSession((state) => state.login);
  const bootstrap = useSession((state) => state.bootstrap);
  const navigate = useNavigate();
  const location = useLocation();
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  const loadStatus = useCallback(() => auth.setupStatus(), []);
  const { data: status, loading, error: statusError } = useResource(loadStatus, null);

  // Only when the server says so. Defaulting to the setup form on an error
  // would offer to create an administrator on an instance that already has
  // one, and the attempt would be refused anyway.
  const needsSetup = status?.superuser_exists === false;

  const form = useForm({
    initialValues: { username: '', password: '' },
    validate: {
      username: (value) => (value.trim() ? null : 'Required'),
      password: (value) => (value ? null : 'Required'),
    },
  });

  const submit = async (values) => {
    setBusy(true);
    setError(null);
    try {
      const run = needsSetup ? bootstrap : login;
      await run(values.username.trim(), values.password);
      navigate(location.state?.from ?? '/channels', { replace: true });
    } catch (failure) {
      setError(failure?.message ?? 'Sign in failed.');
    } finally {
      setBusy(false);
    }
  };

  return (
    <Center h="100%" bg="var(--mantine-color-dark-9)">
      <Paper
        w={340}
        p="lg"
        radius="md"
        withBorder
        bg="var(--mantine-color-dark-8)"
        style={{ borderColor: 'var(--mantine-color-dark-6)' }}
      >
        <form onSubmit={form.onSubmit(submit)}>
          <Stack gap="sm">
            <Stack gap={2} align="center" mb="xs">
              <Box
                style={{
                  display: 'grid',
                  placeItems: 'center',
                  width: 48,
                  height: 48,
                  borderRadius: 12,
                  background: 'var(--mantine-color-accent-9)',
                  color: 'var(--mantine-color-accent-4)',
                }}
              >
                <DolletMark size={32} />
              </Box>
              <Text fw={600} size="md" mt={6}>
                Dollet
              </Text>
              <Text size="xs" c="dimmed">
                {loading
                  ? 'Checking this instance…'
                  : needsSetup
                    ? 'Create the first administrator'
                    : 'Sign in to continue'}
              </Text>
            </Stack>

            {loading && (
              <Center py="md">
                <Loader size="sm" color="accent" />
              </Center>
            )}

            {!loading && needsSetup && (
              <Alert color="accent" variant="light" p="xs" fz="xs">
                This instance has no accounts yet. The account you create here is an
                administrator, and this form will not appear again.
              </Alert>
            )}

            {!loading && statusError && (
              <Alert
                color="yellow"
                variant="light"
                icon={<TriangleAlert size={15} />}
                p="xs"
                fz="xs"
              >
                Could not tell whether this instance has been set up. Signing in will
                still work if it has.
              </Alert>
            )}

            {error && (
              <Alert
                color="red"
                variant="light"
                icon={<TriangleAlert size={15} />}
                p="xs"
                fz="xs"
              >
                {error}
              </Alert>
            )}

            {!loading && (
              <>
                <TextInput
                  label="Username"
                  autoComplete="username"
                  autoFocus
                  {...form.getInputProps('username')}
                />
                <PasswordInput
                  label="Password"
                  autoComplete={needsSetup ? 'new-password' : 'current-password'}
                  {...form.getInputProps('password')}
                />
                <Button type="submit" fullWidth loading={busy} mt="xs">
                  {needsSetup ? 'Create administrator' : 'Sign in'}
                </Button>
              </>
            )}
          </Stack>
        </form>
      </Paper>
    </Center>
  );
}
