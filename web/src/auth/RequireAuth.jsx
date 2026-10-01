import { Center, Loader } from '@mantine/core';
import { Navigate, Outlet, useLocation } from 'react-router-dom';

import { useSession } from './session.js';

/** Gate for every route inside the app shell. */
export function RequireAuth() {
  const status = useSession((state) => state.status);
  const location = useLocation();

  if (status === 'loading') {
    return (
      <Center h="100%">
        <Loader color="accent" />
      </Center>
    );
  }

  if (status === 'anonymous') {
    // `state.from` is what sends the user back where they were aiming after
    // a session expiry mid-navigation.
    return <Navigate to="/login" replace state={{ from: location.pathname }} />;
  }

  return <Outlet />;
}

/** Keeps an already-signed-in user off the login page. */
export function RequireAnonymous() {
  const status = useSession((state) => state.status);
  if (status === 'authenticated') return <Navigate to="/channels" replace />;
  return <Outlet />;
}
