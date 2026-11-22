import { useCallback, useEffect, useState } from 'react';

import { NewSessionDialog } from './components/NewSessionDialog';
import { Agents, Home } from './components/Overview';
import { SessionView } from './components/SessionView';
import { SettingsDialog } from './components/SettingsDialog';
import { Sidebar } from './components/Sidebar';
import { Icon, Spinner } from './components/ui';
import { init, useApp } from './store';

type Modal = { kind: 'new'; workspace?: string } | { kind: 'settings' } | null;

export function App() {
  const ready = useApp((state) => state.ready);
  const fatal = useApp((state) => state.fatal);
  const route = useApp((state) => state.route);
  const toast = useApp((state) => state.toast);
  const sessions = useApp((state) => state.sessions);
  const [modal, setModal] = useState<Modal>(null);

  useEffect(() => {
    void init();
  }, []);

  // Sessions that need the user show up in the tab title.
  useEffect(() => {
    const waiting = sessions.filter((session) => session.status === 'waiting_approval').length;
    document.title = waiting ? `(${waiting}) SWE Agent` : 'SWE Agent';
  }, [sessions]);

  const closeModal = useCallback(() => setModal(null), []);
  const openSettings = useCallback(() => setModal({ kind: 'settings' }), []);
  const newSession = useCallback((workspace?: string) => setModal({ kind: 'new', workspace }), []);

  if (fatal) {
    return (
      <div className="center-message full">
        <Icon name="alert" size={24} />
        <p>{fatal}</p>
        <p className="muted">Start the server with `swe serve` and reload this page.</p>
      </div>
    );
  }
  if (!ready) {
    return (
      <div className="center-message full">
        <Spinner size={20} />
      </div>
    );
  }

  return (
    <div className="app">
      <Sidebar route={route} onNewSession={newSession} onOpenSettings={openSettings} />
      {route.page === 'session' ? (
        <SessionView key={route.id} id={route.id} onOpenSettings={openSettings} />
      ) : route.page === 'agents' ? (
        <Agents onNewSession={() => newSession()} />
      ) : (
        <Home onNewSession={() => newSession()} onOpenSettings={openSettings} />
      )}
      {modal?.kind === 'new' ? (
        <NewSessionDialog initialWorkspace={modal.workspace} onClose={closeModal} onOpenSettings={openSettings} />
      ) : null}
      {modal?.kind === 'settings' ? <SettingsDialog onClose={closeModal} /> : null}
      {toast ? (
        <div key={toast.id} className={`toast toast-${toast.kind}`} role="status">
          {toast.text}
        </div>
      ) : null}
    </div>
  );
}
