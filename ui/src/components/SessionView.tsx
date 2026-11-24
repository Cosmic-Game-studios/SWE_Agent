import { useEffect, useMemo, useState, useSyncExternalStore } from 'react';

import type { SessionMeta } from '../api';
import { STATUS_LABEL, baseName } from '../format';
import { ROOT, SessionStore, pendingApprovals } from '../session';
import { attempt, requestDelete, updateSession, useApp } from '../store';
import { Composer } from './Composer';
import { type PanelTab, RightPanel } from './RightPanel';
import { AgentTimeline } from './Timeline';
import { Button, Icon, Kbd, Spinner, useStickToBottom } from './ui';

function useSessionStore(id: string): SessionStore {
  const store = useMemo(() => new SessionStore(id), [id]);
  useEffect(() => () => store.dispose(), [store]);
  useSyncExternalStore(store.subscribe, store.getVersion);
  return store;
}

function Title({ meta }: { meta: SessionMeta }) {
  const [editing, setEditing] = useState(false);
  const [value, setValue] = useState(meta.title);
  useEffect(() => setValue(meta.title), [meta.title]);
  if (!editing) {
    return (
      <button type="button" className="session-title" onClick={() => setEditing(true)} title="Rename">
        {meta.title}
      </button>
    );
  }
  const commit = () => {
    setEditing(false);
    const title = value.trim();
    if (title && title !== meta.title) void attempt(() => updateSession(meta.id, { title }));
    else setValue(meta.title);
  };
  return (
    <input
      className="session-title-input"
      autoFocus
      value={value}
      aria-label="Session title"
      onChange={(event) => setValue(event.target.value)}
      onBlur={commit}
      onKeyDown={(event) => {
        if (event.key === 'Enter') commit();
        if (event.key === 'Escape') {
          setValue(meta.title);
          setEditing(false);
        }
      }}
    />
  );
}

const SUGGESTIONS = [
  'Explain how this project is structured and how to build and test it.',
  'Find and fix the failing tests.',
  'Review the uncommitted changes and point out problems.',
];

export function SessionView({ id, onOpenSettings }: { id: string; onOpenSettings: () => void }) {
  const store = useSessionStore(id);
  const meta = useApp((state) => state.sessions.find((session) => session.id === id));
  const [panelOpen, setPanelOpen] = useState(() => window.innerWidth >= 1280 && localStorage.getItem('swe-panel') !== 'closed');
  const [tab, setTab] = useState<PanelTab>('changes');
  const view = store.view;
  const root = view.agents[ROOT];
  const { ref, atBottom, scrollToBottom } = useStickToBottom(store.version);

  const togglePanel = () => {
    setPanelOpen((open) => {
      localStorage.setItem('swe-panel', open ? 'closed' : 'open');
      return !open;
    });
  };

  if (store.state === 'error') {
    return (
      <main className="main">
        <div className="center-message">
          <Icon name="alert" size={22} />
          <p>{store.error}</p>
        </div>
      </main>
    );
  }
  if (!meta || store.state === 'loading') {
    return (
      <main className="main">
        <div className="center-message">
          <Spinner size={18} />
        </div>
      </main>
    );
  }

  const running = view.running || meta.status === 'running' || meta.status === 'waiting_approval';
  const approvals = pendingApprovals(view).length;
  const empty = !root?.items.length && !running;

  return (
    <>
      <main className="main">
        <header className="session-header">
          <div className="session-heading">
            <Title meta={meta} />
            <div className="session-sub">
              <span title={meta.workspace}>
                <Icon name="folder" size={12} /> {baseName(meta.workspace)}
              </span>
              {meta.agent === 'orchestrator' ? <span className="tag tag-purple">orchestrator</span> : null}
              {meta.engine !== 'native' ? <span className="tag">{meta.engine}</span> : null}
              {meta.status !== 'idle' ? (
                <span className={`status status-${meta.status}`}>
                  {meta.status === 'running' ? <Spinner size={10} /> : null}
                  {STATUS_LABEL[meta.status]}
                  {meta.status === 'waiting_approval' && approvals > 1 ? ` (${approvals})` : ''}
                </span>
              ) : null}
              {!store.connected ? <span className="status status-error">Reconnecting…</span> : null}
            </div>
          </div>
          <div className="session-actions">
            <Button
              variant="ghost"
              icon="trash"
              aria-label="Delete session"
              title="Delete session"
              onClick={() => requestDelete(meta)}
            />
            <Button
              variant={panelOpen ? 'default' : 'ghost'}
              icon="panel"
              aria-label="Toggle details panel"
              aria-pressed={panelOpen}
              title="Changes, plan and agents"
              onClick={togglePanel}
            />
          </div>
        </header>

        <div className="scroll" ref={ref}>
          <div className="column">
            {empty ? (
              <div className="empty-session">
                <h2>What should the agent do?</h2>
                <p className="muted">
                  It works in <code>{meta.workspace}</code> with {meta.model || meta.engine}. Describe the outcome you want; it will read
                  the code, make the changes and verify them.
                </p>
                <div className="suggestions">
                  {SUGGESTIONS.map((suggestion) => (
                    <button
                      key={suggestion}
                      type="button"
                      className="suggestion"
                      onClick={() => void attempt(() => store.send(suggestion, []))}
                    >
                      {suggestion}
                      <Icon name="arrow-right" size={13} />
                    </button>
                  ))}
                </div>
              </div>
            ) : (
              <AgentTimeline agentId={ROOT} view={view} sessionId={id} checkpoints={running ? undefined : store.checkpoints} />
            )}
            {meta.status === 'interrupted' ? (
              <div className="notice">
                <Icon name="info" size={13} />
                <span>The server stopped while this session was working. Send a message to continue where it left off.</span>
              </div>
            ) : null}
          </div>
        </div>

        <div className="composer-wrap">
          <div className="column">
            {atBottom ? null : (
              <button type="button" className="jump-bottom" onClick={scrollToBottom}>
                <Icon name="chevron-down" size={14} />
                {running ? 'Follow the agent' : 'Jump to latest'}
              </button>
            )}
            <Composer meta={meta} store={store} running={running} onOpenSettings={onOpenSettings} onSent={scrollToBottom} />
            <p className="composer-hint">
              <span>
                <Kbd>↵</Kbd> send
              </span>
              <span>
                <Kbd>Shift</Kbd>
                <Kbd>↵</Kbd> new line
              </span>
              <span>
                <Kbd>/</Kbd> focus
              </span>
            </p>
          </div>
        </div>
      </main>
      {panelOpen ? <RightPanel sessionId={id} view={view} tab={tab} onTab={setTab} /> : null}
    </>
  );
}
