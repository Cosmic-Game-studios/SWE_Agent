import { useMemo } from 'react';

import type { SessionMeta } from '../api';
import { MOD_KEY, STATUS_LABEL, baseName, relativeTime } from '../format';
import { type Route, navigate, requestDelete, useApp } from '../store';
import { Button, Icon, Kbd, Spinner } from './ui';

export function StatusMark({ status }: { status: SessionMeta['status'] }) {
  if (status === 'running') return <Spinner size={11} />;
  return <span className={`dot dot-${status}`} title={STATUS_LABEL[status]} />;
}

function SessionLink({ session, active }: { session: SessionMeta; active: boolean }) {
  return (
    <div className="nav-session-row">
      <a
        className={active ? 'nav-session active' : 'nav-session'}
        href={`#/s/${session.id}`}
        aria-current={active ? 'page' : undefined}
        title={`${session.title}\n${session.model || session.engine} · ${STATUS_LABEL[session.status]}`}
      >
        <span className="nav-session-mark">
          <StatusMark status={session.status} />
        </span>
        <span className="nav-session-title">{session.title}</span>
        <span className="nav-session-time">{relativeTime(session.updated_at).replace(' ago', '')}</span>
      </a>
      <button
        type="button"
        className="nav-session-delete"
        aria-label={`Delete ${session.title}`}
        title="Delete session"
        onClick={() => requestDelete(session)}
      >
        <Icon name="trash" size={13} />
      </button>
    </div>
  );
}

export function Sidebar({
  route,
  onNewSession,
  onOpenSettings,
  onOpenPalette,
}: {
  route: Route;
  onNewSession: (workspace?: string) => void;
  onOpenSettings: () => void;
  onOpenPalette: () => void;
}) {
  const sessions = useApp((state) => state.sessions);
  const connected = useApp((state) => state.connected);
  const version = useApp((state) => state.meta?.version);

  // Group by workspace; groups and their sessions keep "most recent first".
  const groups = useMemo(() => {
    const byWorkspace = new Map<string, SessionMeta[]>();
    for (const session of sessions) {
      const list = byWorkspace.get(session.workspace) ?? [];
      list.push(session);
      byWorkspace.set(session.workspace, list);
    }
    return Array.from(byWorkspace.entries());
  }, [sessions]);

  const attention = sessions.filter((session) => session.status === 'waiting_approval').length;
  const working = sessions.filter((session) => session.status === 'running').length;

  return (
    <nav className="sidebar" aria-label="Sessions">
      <div className="sidebar-top">
        <a className="brand" href="#/" onClick={() => navigate({ page: 'home' })}>
          <span className="brand-mark">
            <Icon name="terminal" size={14} />
          </span>
          SWE Agent
        </a>
        <Button variant="primary" icon="plus" onClick={() => onNewSession()} className="new-session">
          New session
        </Button>
        <button type="button" className="search-trigger" onClick={onOpenPalette} title="Search sessions and commands">
          <Icon name="search" size={14} />
          <span>Search…</span>
          <Kbd>{MOD_KEY} K</Kbd>
        </button>
        <a className={route.page === 'agents' ? 'nav-link active' : 'nav-link'} href="#/agents">
          <Icon name="grid" size={14} />
          <span>All agents</span>
          {attention ? <span className="badge badge-yellow">{attention}</span> : working ? <span className="badge">{working}</span> : null}
        </a>
      </div>

      <div className="sidebar-list">
        {groups.length === 0 ? <p className="sidebar-empty">No sessions yet.</p> : null}
        {groups.map(([workspace, list]) => (
          <section key={workspace} className="nav-group">
            <header className="nav-group-header" title={workspace}>
              <Icon name="folder" size={12} />
              <span>{baseName(workspace)}</span>
              <em className="nav-group-count">{list.length}</em>
              <button
                type="button"
                className="nav-group-add"
                aria-label={`New session in ${baseName(workspace)}`}
                title="New session in this workspace"
                onClick={() => onNewSession(workspace)}
              >
                <Icon name="plus" size={12} />
              </button>
            </header>
            {list.map((session) => (
              <SessionLink key={session.id} session={session} active={route.page === 'session' && route.id === session.id} />
            ))}
          </section>
        ))}
      </div>

      <div className="sidebar-bottom">
        <button type="button" className="nav-link" onClick={onOpenSettings}>
          <Icon name="settings" size={14} />
          <span>Settings</span>
        </button>
        <span className="sidebar-meta">
          {connected ? null : <span className="status status-error">Offline</span>}
          {version ? <span>v{version}</span> : null}
        </span>
      </div>
    </nav>
  );
}
