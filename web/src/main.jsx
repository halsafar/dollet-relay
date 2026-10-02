import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { BrowserRouter } from 'react-router-dom';
import { Notifications } from '@mantine/notifications';

import '@mantine/core/styles.css';
import '@mantine/notifications/styles.css';
import './styles.css';

import { App } from './App.jsx';
import { AppearanceProvider } from './AppearanceProvider.jsx';
import { restoreSession } from './auth/session.js';

restoreSession();

createRoot(document.getElementById('root')).render(
  <StrictMode>
    <AppearanceProvider>
      <Notifications position="bottom-right" limit={4} />
      <BrowserRouter>
        <App />
      </BrowserRouter>
    </AppearanceProvider>
  </StrictMode>,
);
