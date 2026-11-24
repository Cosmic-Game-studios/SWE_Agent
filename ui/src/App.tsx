import { useCallback, useEffect, useState } from 'react';

import { CommandPalette } from './components/CommandPalette';
import { NewSessionDialog } from './components/NewSessionDialog';
import { Agents, Home } from './components/Overview';
import { SessionView } from './components/SessionView';
import { SettingsDialog } from './components/SettingsDialog';
import { Sidebar } from './components/Sidebar';
import { Button, Dialog, Icon, Spinner } from './components/ui';
import { attempt, deleteSession, init, requestDelete, useApp } from './store';

type Modal = { kind: 'new'; workspace?: string } | { kind: 'settings' } | { kind: 'palette' } | null;

function DeleteDialog() {
  const session = useApp((state) => state.pendingDelete);
  const [busy, setBusy] = useState(false);
  const close = useCallback(() => requestDelete(null), []);
  if (!session) return null;
  const working = session.status === 'running' || session.status === 'waiting_approval';
  const confirm = async () => {
    setBusy(true);
    await attempt(() => deleteSession(session.id));
    setBusy(false);
    close();
  };
  return (
    <Dialog
      title="Delete session?"
      onClose={close}
      footer={
        <>
          <Button onClick={close}>Cancel</Button>
          <Button variant="danger" icon="trash" busy={busy} data-autofocus onClick={() => void confirm()}>
            Delete session
          </Button>
        </>
      }
    >
      <p>
        <strong>{session.title}</strong> and its history will be removed for good. Files in the workspace are not touched.
      </p>
      {working ? (
        <p className="confirm-warning">
          <Icon name="alert" size={13} /> This session is still working. Deleting it stops the agent.
        </p>
      ) : null}
    </Dialog>
  );
}

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
  const openPalette = useCallback(() => setModal({ kind: 'palette' }), []);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && !event.altKey && event.key.toLowerCase() === 'k') {
        event.preventDefault();
        setModal((current) => (current?.kind === 'palette' ? null : { kind: 'palette' }));
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

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
      <Sidebar route={route} onNewSession={newSession} onOpenSettings={openSettings} onOpenPalette={openPalette} />
      {route.page === 'session' ? (
        <SessionView key={route.id} id={route.id} onOpenSettings={openSettings} />
      ) : route.page === 'agents' ? (
        <Agents onNewSession={() => newSession()} />
      ) : (
        <Home onNewSession={() => newSession()} onOpenSettings={openSettings} onOpenPalette={openPalette} />
      )}
      {modal?.kind === 'new' ? (
        <NewSessionDialog initialWorkspace={modal.workspace} onClose={closeModal} onOpenSettings={openSettings} />
      ) : null}
      {modal?.kind === 'settings' ? <SettingsDialog onClose={closeModal} /> : null}
      {modal?.kind === 'palette' ? (
        <CommandPalette onClose={closeModal} onNewSession={() => newSession()} onOpenSettings={openSettings} />
      ) : null}
      <DeleteDialog />
      {toast ? (
        <div key={toast.id} className={`toast toast-${toast.kind}`} role="status">
          <Icon name={toast.kind === 'error' ? 'alert' : 'check'} size={14} />
          {toast.text}
        </div>
      ) : null}
    </div>
  );
}
