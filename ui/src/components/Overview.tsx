// Landing page and the "all agents" board: every session across every
// workspace at a glance, so several agents can work while you watch one place.

import { useEffect, useState } from 'react';

import type { SessionMeta } from '../api';
import { STATUS_LABEL, baseName, formatTokens, modeLabel, relativeTime } from '../format';
import { useApp } from '../store';
import { StatusMark } from './Sidebar';
import { Button, Icon } from './ui';

function SessionCard({ session }: { session: SessionMeta }) {
  return (
    <a className={`card card-${session.status}`} href={`#/s/${session.id}`}>
      <div className="card-top">
        <StatusMark status={session.status} />
        <span className="card-status">{STATUS_LABEL[session.status]}</span>
        <span className="card-time">{relativeTime(session.updated_at)}</span>
      </div>
      <div className="card-title">{session.title}</div>
      {session.status === 'error' && session.error ? <div className="card-error">{session.error}</div> : null}
      <div className="card-meta">
        <span title={session.workspace}>
          <Icon name="folder" size={11} /> {baseName(session.workspace)}
        </span>
        <span>{session.model || session.engine}</span>
        <span>{modeLabel(session.mode)}</span>
        {session.usage.input_tokens + session.usage.output_tokens > 0 ? (
          <span>{formatTokens(session.usage.input_tokens + session.usage.output_tokens)} tokens</span>
        ) : null}
      </div>
    </a>
  );
}

function Group({ title, sessions, hint }: { title: string; sessions: SessionMeta[]; hint?: string }) {
  if (!sessions.length) return null;
  return (
    <section className="board-group">
      <h2>
        {title} <span className="muted">{sessions.length}</span>
      </h2>
      {hint ? <p className="muted board-hint">{hint}</p> : null}
      <div className="cards">
        {sessions.map((session) => (
          <SessionCard key={session.id} session={session} />
        ))}
      </div>
    </section>
  );
}

/** Re-render periodically so relative times stay honest. */
function useTick(ms: number) {
  const [, setTick] = useState(0);
  useEffect(() => {
    const timer = setInterval(() => setTick((value) => value + 1), ms);
    return () => clearInterval(timer);
  }, [ms]);
}

export function Agents({ onNewSession }: { onNewSession: () => void }) {
  const sessions = useApp((state) => state.sessions);
  useTick(30_000);
  const attention = sessions.filter((s) => s.status === 'waiting_approval' || s.status === 'error' || s.status === 'interrupted');
  const working = sessions.filter((s) => s.status === 'running');
  const idle = sessions.filter((s) => s.status === 'idle');
  return (
    <main className="main">
      <header className="page-header">
        <div>
          <h1>All agents</h1>
          <p className="muted">Every session across your workspaces. They keep working while this page is closed.</p>
        </div>
        <Button variant="primary" icon="plus" onClick={onNewSession}>
          New session
        </Button>
      </header>
      <div className="scroll">
        <div className="board">
          {!sessions.length ? (
            <div className="center-message">
              <p className="muted">No sessions yet.</p>
            </div>
          ) : null}
          <Group title="Needs you" sessions={attention} hint="Waiting for an approval, failed, or interrupted by a restart." />
          <Group title="Working" sessions={working} />
          <Group title="Idle" sessions={idle} />
        </div>
      </div>
    </main>
  );
}

export function Home({ onNewSession, onOpenSettings }: { onNewSession: () => void; onOpenSettings: () => void }) {
  const sessions = useApp((state) => state.sessions);
  const providers = useApp((state) => state.providers);
  const engines = useApp((state) => state.meta?.engines ?? []);
  const hasProvider = providers.some((provider) => provider.key.source !== 'missing');
  const hasCli = engines.some((engine) => engine.id !== 'native' && engine.installed);
  const recent = sessions.slice(0, 6);
  return (
    <main className="main">
      <div className="scroll">
        <div className="home">
          <h1>SWE Agent</h1>
          <p className="lead">
            An autonomous software engineer that works in your repositories with the model you choose: it reads the code, plans, edits,
            runs and verifies, on its own or as an orchestrator leading parallel sub-agents.
          </p>
          {hasProvider || hasCli ? (
            <Button variant="primary" icon="plus" onClick={onNewSession}>
              New session
            </Button>
          ) : (
            <div className="steps">
              <div className="step">
                <span className="step-number">1</span>
                <div>
                  <strong>Connect a model</strong>
                  <p className="muted">
                    Add an API key for Anthropic, OpenAI, Google Gemini or OpenRouter. Any OpenAI-compatible endpoint, including local
                    models, works too.
                  </p>
                  <Button variant="primary" icon="key" onClick={onOpenSettings}>
                    Add an API key
                  </Button>
                </div>
              </div>
              <div className="step">
                <span className="step-number">2</span>
                <div>
                  <strong>Point it at a project</strong>
                  <p className="muted">Create a session for a folder and say what you want built, fixed or changed.</p>
                </div>
              </div>
            </div>
          )}
          {recent.length ? (
            <section className="board-group">
              <h2>Recent</h2>
              <div className="cards">
                {recent.map((session) => (
                  <SessionCard key={session.id} session={session} />
                ))}
              </div>
            </section>
          ) : null}
        </div>
      </div>
    </main>
  );
}
