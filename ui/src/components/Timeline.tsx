// The conversation: messages, reasoning, tool calls with their results,
// approvals, and sub-agents nested under the call that started them.

import { memo, useState } from 'react';

import { type ApprovalDecision, type ToolKind, api } from '../api';
import { OUTCOME_LABEL, formatDuration, formatTokens, usageSummary } from '../format';
import { type AgentView, type Item, ROOT, type SessionView, type ToolView } from '../session';
import { attempt } from '../store';
import { DiffStat, InlineDiff } from './DiffView';
import { Markdown } from './Markdown';
import { Button, CopyButton, Icon, type IconName, Spinner } from './ui';

function toolIcon(tool: ToolView): IconName {
  switch (tool.kind) {
    case 'edit':
      return 'edit';
    case 'execute':
      return 'terminal';
    case 'network':
      return 'globe';
    case 'agent':
      return 'bot';
    case 'meta':
      return 'list';
    case 'read':
      return /grep|glob|search/i.test(tool.name) ? 'search' : 'file';
    default:
      return guessKind(tool.name) === 'execute' ? 'terminal' : 'file';
  }
}

function guessKind(name: string): ToolKind {
  if (/bash|shell|exec/i.test(name)) return 'execute';
  if (/write|edit|patch/i.test(name)) return 'edit';
  if (name === 'task') return 'agent';
  return 'read';
}

/** Short text for the header when the server has not sent a summary yet. */
function fallbackSummary(tool: ToolView): string {
  const input = tool.input;
  for (const key of ['command', 'path', 'file_path', 'pattern', 'url', 'description', 'query']) {
    const value = input[key];
    if (typeof value === 'string' && value) return value.split('\n')[0] ?? '';
  }
  return '';
}

function ApprovalPrompt({ tool, sessionId }: { tool: ToolView; sessionId: string }) {
  const approval = tool.approval!;
  const [busy, setBusy] = useState<ApprovalDecision | null>(null);
  const [denying, setDenying] = useState(false);
  const [feedback, setFeedback] = useState('');
  const decide = async (decision: ApprovalDecision) => {
    setBusy(decision);
    await attempt(() => api.resolveApproval(sessionId, approval.requestId, decision, feedback.trim() || undefined));
    setBusy(null);
  };
  return (
    <div className="approval" role="group" aria-label="Approval needed">
      <div className="approval-reason">
        <Icon name="shield" size={14} />
        <span>{approval.reason}</span>
      </div>
      {denying ? (
        <form
          className="approval-deny"
          onSubmit={(event) => {
            event.preventDefault();
            void decide('deny');
          }}
        >
          <input
            autoFocus
            value={feedback}
            onChange={(event) => setFeedback(event.target.value)}
            placeholder="Tell the agent what to do instead (optional)"
            aria-label="Feedback for the agent"
          />
          <Button variant="danger" small busy={busy === 'deny'} type="submit">
            Deny
          </Button>
          <Button variant="ghost" small onClick={() => setDenying(false)}>
            Back
          </Button>
        </form>
      ) : (
        <div className="approval-actions">
          <Button variant="primary" small busy={busy === 'allow'} disabled={busy !== null} onClick={() => decide('allow')}>
            Allow
          </Button>
          {approval.alwaysLabel ? (
            <Button small busy={busy === 'allow_always'} disabled={busy !== null} onClick={() => decide('allow_always')}>
              {approval.alwaysLabel}
            </Button>
          ) : null}
          <Button variant="ghost" small disabled={busy !== null} onClick={() => setDenying(true)}>
            Deny…
          </Button>
        </div>
      )}
    </div>
  );
}

function Output({ text, live }: { text: string; live?: boolean }) {
  if (!text) return null;
  return <pre className={live ? 'output output-live' : 'output'}>{text}</pre>;
}

function ToolBody({ tool, view, sessionId }: { tool: ToolView; view: SessionView; sessionId: string }) {
  const child = tool.childAgentId ? view.agents[tool.childAgentId] : undefined;
  if (child) {
    return (
      <div className="tool-body">
        <div className="agent-meta">
          <span className="tag">{child.agentType}</span>
          <span>{child.model}</span>
          {child.worktree ? (
            <span title={child.worktree}>
              <Icon name="branch" size={12} /> worktree
            </span>
          ) : null}
          {child.usage ? <span>{usageSummary(child.usage)}</span> : null}
        </div>
        {typeof tool.input.prompt === 'string' ? (
          <details className="fold">
            <summary>Task given to the agent</summary>
            <pre className="output">{tool.input.prompt}</pre>
          </details>
        ) : null}
        <AgentTimeline agentId={child.id} view={view} sessionId={sessionId} nested />
        {tool.status === 'error' ? <Output text={tool.output} /> : null}
      </div>
    );
  }
  const display = tool.display;
  return (
    <div className="tool-body">
      {display?.kind === 'diff' ? (
        <InlineDiff diff={display.diff} />
      ) : (
        <>
          {tool.kind === 'execute' && typeof tool.input.command === 'string' ? (
            <div className="copyable">
              <pre className="command">
                <span className="prompt">$</span> {tool.input.command}
              </pre>
              <CopyButton text={tool.input.command} label="Copy command" className="copy-float" />
            </div>
          ) : tool.kind === 'edit' && tool.status !== 'done' ? (
            <EditPreview tool={tool} />
          ) : Object.keys(tool.input).length && tool.kind !== 'read' ? (
            <details className="fold">
              <summary>Arguments</summary>
              <pre className="output">{JSON.stringify(tool.input, null, 2)}</pre>
            </details>
          ) : null}
          {tool.status === 'running' ? <Output text={tool.live} live /> : <Output text={tool.output} />}
        </>
      )}
      {display?.kind === 'diff' && tool.status === 'error' ? <Output text={tool.output} /> : null}
    </div>
  );
}

/** What an edit is about to do, shown while it waits for approval. */
function EditPreview({ tool }: { tool: ToolView }) {
  const input = tool.input;
  if (typeof input.old_string === 'string' && typeof input.new_string === 'string') {
    const removed = input.old_string.split('\n').map((line) => `-${line}`);
    const added = input.new_string.split('\n').map((line) => `+${line}`);
    return <InlineDiff diff={['--- a', '+++ b', '@@ -1 +1 @@', ...removed, ...added].join('\n')} />;
  }
  if (typeof input.content === 'string') {
    const added = input.content.split('\n').map((line) => `+${line}`);
    return <InlineDiff diff={['--- /dev/null', '+++ b', '@@ -0,0 +1 @@', ...added].join('\n')} />;
  }
  return null;
}

function ToolCard({ tool, view, sessionId }: { tool: ToolView; view: SessionView; sessionId: string }) {
  const needsApproval = Boolean(tool.approval && !tool.approval.decision && tool.status === 'waiting');
  const child = tool.childAgentId ? view.agents[tool.childAgentId] : undefined;
  const active = tool.status === 'running' || tool.status === 'streaming';
  const defaultOpen =
    needsApproval ||
    (child ? active : tool.kind === 'edit' || tool.kind === 'execute' || tool.status === 'error') ||
    (tool.kind === null && tool.status === 'error');
  const [override, setOverride] = useState<boolean | null>(null);
  const open = needsApproval || (override ?? defaultOpen);
  const summary = tool.summary || fallbackSummary(tool);
  const diff = tool.display?.kind === 'diff' ? tool.display : null;
  const command = tool.display?.kind === 'command' ? tool.display : null;

  return (
    <div className={`tool tool-${tool.status}${needsApproval ? ' tool-approval' : ''}`}>
      <button type="button" className="tool-header" onClick={() => setOverride(!open)} aria-expanded={open}>
        <Icon name={toolIcon(tool)} size={14} className="tool-icon" />
        <span className="tool-name">{child ? 'agent' : tool.name || 'tool'}</span>
        <span className="tool-summary" title={summary}>
          {child ? child.description : summary}
        </span>
        <span className="tool-status">
          {diff ? <DiffStat added={diff.added} removed={diff.removed} /> : null}
          {command?.background_id ? <span className="tag">background {command.background_id}</span> : null}
          {command && typeof command.exit_code === 'number' && command.exit_code !== 0 ? (
            <span className="tag tag-red">exit {command.exit_code}</span>
          ) : null}
          {tool.status === 'streaming' ? (
            <span className="muted">{tool.inputChars > 0 ? `${formatTokens(tool.inputChars)} chars` : 'preparing'}</span>
          ) : null}
          {needsApproval ? <span className="tag tag-yellow">needs approval</span> : null}
          {active ? <Spinner size={12} /> : null}
          {tool.status === 'done' && tool.durationMs !== undefined && tool.durationMs >= 1000 ? (
            <span className="muted">{formatDuration(tool.durationMs)}</span>
          ) : null}
          {tool.status === 'error' ? <Icon name="x" size={14} className="error" /> : null}
          {tool.status === 'done' ? <Icon name="check" size={14} className="ok" /> : null}
          <Icon name={open ? 'chevron-down' : 'chevron-right'} size={14} className="muted" />
        </span>
      </button>
      {needsApproval ? <ApprovalPrompt tool={tool} sessionId={sessionId} /> : null}
      {open ? <ToolBody tool={tool} view={view} sessionId={sessionId} /> : null}
    </div>
  );
}

function Reasoning({ text, live }: { text: string; live?: boolean }) {
  return (
    <details className="reasoning" open={live ? true : undefined}>
      <summary>
        <Icon name="zap" size={12} />
        {live ? 'Thinking…' : 'Thought'}
      </summary>
      <div className="reasoning-text">{text}</div>
    </details>
  );
}

interface ItemProps {
  item: Item;
  view: SessionView;
  sessionId: string;
}

const ItemView = memo(
  function ItemView({ item, view, sessionId }: ItemProps) {
    switch (item.kind) {
      case 'user':
        return (
          <div className="message user">
            {item.images.length ? (
              <div className="attachments">
                {item.images.map((image, index) => (
                  <img key={index} src={`data:${image.media_type};base64,${image.data}`} alt="Attached" />
                ))}
              </div>
            ) : null}
            <div className="user-text">{item.text}</div>
          </div>
        );
      case 'assistant':
        return (
          <div className="message assistant">
            {item.blocks.map((block, index) => {
              if (block.type === 'text') return <Markdown key={index} text={block.text} />;
              if (block.type === 'reasoning') return <Reasoning key={index} text={block.text} />;
              const tool = view.tools[block.id];
              return tool ? <ToolCard key={block.id} tool={tool} view={view} sessionId={sessionId} /> : null;
            })}
            {item.note ? <div className="message-note">{item.note}</div> : null}
          </div>
        );
      case 'notice':
        return (
          <div className="notice">
            <Icon name="info" size={13} />
            <span>{item.text}</span>
          </div>
        );
      case 'error':
        return (
          <div className="error-box" role="alert">
            <Icon name="alert" size={14} />
            <span>{item.message}</span>
          </div>
        );
      case 'compacted':
        return (
          <details className="compacted">
            <summary>
              <Icon name="archive" size={13} />
              Earlier conversation summarised to free context
              {item.tokensBefore > 0 ? ` (was ${formatTokens(item.tokensBefore)} tokens)` : ''}
            </summary>
            <Markdown text={item.summary} />
          </details>
        );
      case 'turn':
        return (
          <div className={`turn-end turn-${item.outcome}`}>
            <span>{OUTCOME_LABEL[item.outcome]}</span>
            {item.usage.input_tokens + item.usage.output_tokens > 0 ? <span>{usageSummary(item.usage)}</span> : null}
          </div>
        );
    }
  },
  (previous, next) => {
    if (previous.item !== next.item || previous.sessionId !== next.sessionId) return false;
    if (next.item.kind !== 'assistant') return true;
    for (const block of next.item.blocks) {
      if (block.type !== 'tool') continue;
      const before = previous.view.tools[block.id];
      const after = next.view.tools[block.id];
      if (before !== after) return false;
      // A sub-agent's timeline lives elsewhere in the view; follow any change.
      if (after?.childAgentId && previous.view !== next.view) return false;
    }
    return true;
  },
);

function LiveArea({ agent, view, sessionId }: { agent: AgentView; view: SessionView; sessionId: string }) {
  const running = agent.id === ROOT ? view.running : agent.status === 'running';
  const hasActiveTool = Object.values(view.tools).some(
    (tool) => tool.agentId === agent.id && (tool.status === 'running' || tool.status === 'waiting' || tool.status === 'streaming'),
  );
  const streaming = Boolean(agent.streamingText || agent.streamingReasoning || agent.pendingTools.length);
  return (
    <>
      {agent.retrying ? (
        <div className="notice notice-retry">
          <Spinner size={12} />
          <span>
            Retrying (attempt {agent.retrying.attempt}) in {formatDuration(agent.retrying.delayMs)}: {agent.retrying.message}
          </span>
        </div>
      ) : null}
      {streaming ? (
        <div className="message assistant live">
          {agent.streamingReasoning ? <Reasoning text={agent.streamingReasoning} live={!agent.streamingText} /> : null}
          {agent.streamingText ? <Markdown text={agent.streamingText} /> : null}
          {agent.pendingTools.map((id) => {
            const tool = view.tools[id];
            return tool ? <ToolCard key={id} tool={tool} view={view} sessionId={sessionId} /> : null;
          })}
        </div>
      ) : null}
      {running && !streaming && !hasActiveTool && !agent.retrying ? (
        <div className="working" aria-live="polite">
          <Spinner size={12} />
          <span>Working…</span>
        </div>
      ) : null}
    </>
  );
}

export function AgentTimeline({
  agentId,
  view,
  sessionId,
  nested,
}: {
  agentId: string;
  view: SessionView;
  sessionId: string;
  nested?: boolean;
}) {
  const agent = view.agents[agentId];
  if (!agent) return null;
  return (
    <div className={nested ? 'timeline nested' : 'timeline'}>
      {agent.items.map((item) => (
        <ItemView key={item.key} item={item} view={view} sessionId={sessionId} />
      ))}
      <LiveArea agent={agent} view={view} sessionId={sessionId} />
    </div>
  );
}
