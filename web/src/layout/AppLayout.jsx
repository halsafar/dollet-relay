import { Outlet } from 'react-router-dom';

import { Sidebar } from './Sidebar.jsx';
import { ErrorBoundary } from '../components/ErrorBoundary.jsx';
import classes from './AppLayout.module.css';

export function AppLayout() {
  return (
    <div className={classes.shell}>
      <Sidebar />
      <main className={classes.main}>
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
