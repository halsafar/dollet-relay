import { Outlet } from 'react-router-dom';
import { Burger, Drawer } from '@mantine/core';
import { useDisclosure } from '@mantine/hooks';

import { Brand, Sidebar } from './Sidebar.jsx';
import { ErrorBoundary } from '../components/ErrorBoundary.jsx';
import classes from './AppLayout.module.css';

/**
 * The sidebar is docked beside the content on a wide window and offered from
 * a burger on a narrow one, where docked it would take most of the width.
 * The same `Sidebar` renders in whichever place the width calls for; the
 * stylesheet decides which is shown.
 */
export function AppLayout() {
  const [opened, { open, close }] = useDisclosure(false);

  return (
    <div className={classes.shell}>
      <div className={classes.dock}>
        <Sidebar />
      </div>

      <Drawer
        opened={opened}
        onClose={close}
        position="left"
        size={232}
        padding={0}
        withCloseButton={false}
        overlayProps={{ backgroundOpacity: 0.7 }}
        styles={{ body: { height: '100%' } }}
      >
        <Sidebar onNavigate={close} />
      </Drawer>

      <main className={classes.main}>
        <div className={classes.topBar}>
          <Burger opened={opened} onClick={open} size="sm" aria-label="Open navigation" />
          <Brand />
        </div>
        <ErrorBoundary>
          <Outlet />
        </ErrorBoundary>
      </main>
    </div>
  );
}

/**
 * The frame every screen renders into: title on the left, that screen's
 * controls on the right. There is no global top bar, because none of these
 * screens share controls.
 */
export function Page({ title, subtitle, actions, children }) {
  return (
    <div className={classes.content}>
      <header className={classes.header}>
        <div>
          <h1 className={classes.title}>{title}</h1>
          {subtitle && <p className={classes.subtitle}>{subtitle}</p>}
        </div>
        {actions && <div className={classes.actions}>{actions}</div>}
      </header>
      {children}
    </div>
  );
}
