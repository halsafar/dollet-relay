import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { BrowserRouter } from 'react-router-dom';
import { MantineProvider } from '@mantine/core';
import { Notifications } from '@mantine/notifications';

import '@mantine/core/styles.css';
import '@mantine/notifications/styles.css';
import './styles.css';

import { App } from './App.jsx';
import { theme } from './theme.js';
import { restoreSession } from './auth/session.js';

restoreSession();

createRoot(document.getElementById('root')).render(
  <StrictMode>
    <MantineProvider theme={theme} forceColorScheme="dark">
      <Notifications position="bottom-right" limit={4} />
      <BrowserRouter>
        <App />
      </BrowserRouter>
    </MantineProvider>
  </StrictMode>,
);
