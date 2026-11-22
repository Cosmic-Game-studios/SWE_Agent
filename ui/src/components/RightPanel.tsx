// Side panel of a session: workspace changes, the agent's plan, the agent tree.

import { useEffect, useMemo, useRef, useState } from 'react';

import { type WorkspaceDiff, api } from '../api';
import { usageSummary } from '../format';
import { type AgentView, ROOT, type SessionView } from '../session';
import { DiffFileView, DiffStat, parseDiff } from './DiffView';
import { Button, Icon, Spinner } from './ui';

export type PanelTab = 'changes' | 'plan' | 'agents';

function Changes({ sessionId, view }: { sessionId: string; view: SessionView }) {
  const [diff, setDiff] = useState<WorkspaceDiff | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const request = useRef(0);

  // The workspace can change whenever an edit or a command finishes.
  const mutations = useMemo(
    () =>
      Object.values(view.tools).filter(
        (tool) => (tool.status === 'done' || tool.status === 'error') && (tool.kind === 'edit' || tool.kind === 'execute' || tool.kind === 'agent'),
      ).length,
    [view.tools],
  );

  const load = async () => {
    const id = ++request.current;
    setLoading(true);
    try {
      const next = await api.diff(sessionId);
      if (id === request.current) {
        setDiff(next);
        setError(null);
      }
    } catch (failure) {
      if (id === request.current) setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      if (id === request.current) setLoading(false);
    }
  };

  useEffect(() => {
    const timer = setTimeout(load, 250);
    return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId, mutations]);

  const files = useMemo(() => (diff ? parseDiff(diff.diff) : []), [diff]);
  const totals = files.reduce((sum, file) => ({ added: sum.added + file.added, removed: sum.removed + file.removed }), {
    added: 0,
    removed: 0,
  });

  return (
    <div className="panel-content">
      <div className="panel-toolbar">
        {diff?.branch ? (
          <span className="panel-branch" title="Current branch">
            <Icon name="branch" size={13} /> {diff.branch}
          </span>
        ) : (
          <span />
        )}
        <span className="panel-toolbar-right">
          {files.length ? (
            <span className="muted">
              {files.length} file{files.length === 1 ? '' : 's'} <DiffStat added={totals.added} removed={totals.removed} />
            </span>
          ) : null}
          <Button variant="ghost" small icon="refresh" busy={loading} onClick={load} aria-label="Refresh changes" title="Refresh" />
        </span>
      </div>
      {error ? <div className="panel-empty error">{error}</div> : null}
      {!diff && loading ? (
        <div className="panel-empty">
          <Spinner /> Loading changes…
        </div>
      ) : null}
      {diff && !diff.is_git ? (
        <div className="panel-empty">
          <p>This workspace is not a git repository.</p>
          <p className="muted">Changes are shown relative to the last commit, so they need git. Each edit still shows its own diff in the conversation.</p>
        </div>
      ) : null}
      {diff?.is_git && !files.length && !error ? (
        <div className="panel-empty">
          <p>No uncommitted changes.</p>
          <p className="muted">Everything the agent changes appears here as a diff against the last commit.</p>
        </div>
      ) : null}
      {files.map((file) => (
        <DiffFileView key={file.path} file={file} defaultOpen={files.length <= 6} />
      ))}
      {diff?.truncated ? <div className="panel-empty muted">The diff is too large to show completely.</div> : null}
    </div>
  );
}

function Plan({ view }: { view: SessionView }) {
  if (!view.todos.length) {
    return (
      <div className="panel-content">
        <div className="panel-empty">
          <p>No plan yet.</p>
          <p className="muted">For multi-step work the agent keeps a todo list here and ticks items off as it goes.</p>
        </div>
      </div>
    );
  }
  const done = view.todos.filter((todo) => todo.status === 'completed').length;
  return (
    <div className="panel-content">
      <div className="panel-toolbar">
        <span className="muted">
          {done} of {view.todos.length} done
        </span>
        <span className="meter meter-wide">
          <span style={{ width: `${Math.round((done / view.todos.length) * 100)}%` }} />
        </span>
      </div>
      <ul className="todos">
        {view.todos.map((todo, index) => (
          <li key={index} className={`todo todo-${todo.status}`}>
            <span className="todo-mark">
              {todo.status === 'completed' ? <Icon name="check" size={12} /> : todo.status === 'in_progress' ? <Spinner size={11} /> : null}
            </span>
            <span>{todo.content}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function AgentRow({ agent, view, depth }: { agent: AgentView; view: SessionView; depth: number }) {
  const children = Object.values(view.agents).filter((candidate) => candidate.parentId === agent.id);
  const running = agent.id === ROOT ? view.running : agent.status === 'running';
  const waiting = Object.values(view.tools).some(
    (tool) => tool.agentId === agent.id && tool.approval && !tool.approval.decision && tool.status === 'waiting',
  );
  const activeTool = Object.values(view.tools).find((tool) => tool.agentId === agent.id && tool.status === 'running' && !tool.childAgentId);
  const state = waiting ? 'waiting' : running ? 'running' : agent.status;
  return (
    <>
      <li className="agent-row" style={{ paddingLeft: 12 + depth * 18 }}>
        <span className={`dot dot-${state}`} />
        <div className="agent-row-main">
          <div className="agent-row-title">
            {agent.id === ROOT ? 'Main agent' : agent.description}
            {agent.id !== ROOT ? <span className="tag">{agent.agentType}</span> : null}
          </div>
          <div className="agent-row-detail muted">
            {waiting
              ? 'Waiting for approval'
              : activeTool
                ? `${activeTool.name} ${activeTool.summary}`
                : running
                  ? 'Working'
                  : agent.id === ROOT
                    ? 'Idle'
                    : agent.status}
            {agent.usage ? ` · ${usageSummary(agent.usage)}` : ''}
          </div>
        </div>
      </li>
      {children.map((child) => (
        <AgentRow key={child.id} agent={child} view={view} depth={depth + 1} />
      ))}
    </>
  );
}

function Agents({ view }: { view: SessionView }) {
  const root = view.agents[ROOT];
  const count = Object.keys(view.agents).length - 1;
  return (
    <div className="panel-content">
      {count === 0 ? (
        <div className="panel-empty">
          <p>No sub-agents yet.</p>
          <p className="muted">
            Agents can delegate work to sub-agents that run in parallel. The orchestrator profile is built around that: it plans, hands
            out the pieces and integrates the results.
          </p>
        </div>
      ) : null}
      <ul className="agent-tree">{root ? <AgentRow agent={root} view={view} depth={0} /> : null}</ul>
    </div>
  );
}

export function RightPanel({
  sessionId,
  view,
  tab,
  onTab,
}: {
  sessionId: string;
  view: SessionView;
  tab: PanelTab;
  onTab: (tab: PanelTab) => void;
}) {
  const agentCount = Object.keys(view.agents).length - 1;
  const openTodos = view.todos.filter((todo) => todo.status !== 'completed').length;
  const tabs: { id: PanelTab; label: string; badge?: number }[] = [
    { id: 'changes', label: 'Changes' },
    { id: 'plan', label: 'Plan', badge: openTodos || undefined },
    { id: 'agents', label: 'Agents', badge: agentCount || undefined },
  ];
  return (
    <aside className="right-panel" aria-label="Session details">
      <div className="tabs" role="tablist">
        {tabs.map((entry) => (
          <button
            key={entry.id}
            type="button"
            role="tab"
            aria-selected={tab === entry.id}
            className={tab === entry.id ? 'tab active' : 'tab'}
            onClick={() => onTab(entry.id)}
          >
            {entry.label}
            {entry.badge ? <span className="badge">{entry.badge}</span> : null}
          </button>
        ))}
      </div>
      {tab === 'changes' ? <Changes sessionId={sessionId} view={view} /> : null}
      {tab === 'plan' ? <Plan view={view} /> : null}
      {tab === 'agents' ? <Agents view={view} /> : null}
    </aside>
  );
}
