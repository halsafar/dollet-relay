import { Navigate, Route, Routes } from 'react-router-dom';

import { AppLayout } from './layout/AppLayout.jsx';
import { RequireAnonymous, RequireAuth } from './auth/RequireAuth.jsx';
import { ErrorBoundary } from './components/ErrorBoundary.jsx';
import { Login } from './pages/Login.jsx';
import { Settings } from './pages/Settings.jsx';
import { Users } from './pages/Users.jsx';
import { Channels } from './pages/Channels.jsx';
import { Groups } from './pages/Groups.jsx';
import { Connect } from './pages/Connect.jsx';
import { Guide } from './pages/Guide.jsx';
import { Sources } from './pages/Sources.jsx';
import { Logos } from './pages/Logos.jsx';
import { Notifications } from './pages/Notifications.jsx';
import { Stats } from './pages/Stats.jsx';
import { NotFound } from './pages/NotFound.jsx';

export function App() {
  return (
    <ErrorBoundary>
      <Routes>
        <Route element={<RequireAnonymous />}>
          <Route path="/login" element={<Login />} />
        </Route>

        <Route element={<RequireAuth />}>
          <Route element={<AppLayout />}>
            <Route index element={<Navigate to="/channels" replace />} />
            <Route path="/channels" element={<Channels />} />
            <Route path="/groups" element={<Groups />} />
            <Route path="/connect" element={<Connect />} />
            <Route path="/guide" element={<Guide />} />
            <Route path="/sources" element={<Sources />} />
            <Route path="/logos" element={<Logos />} />
            <Route path="/notifications" element={<Notifications />} />
            <Route path="/stats" element={<Stats />} />
            <Route path="/users" element={<Users />} />
            <Route path="/settings" element={<Settings />} />
            <Route path="*" element={<NotFound />} />
          </Route>
        </Route>
      </Routes>
    </ErrorBoundary>
  );
}
