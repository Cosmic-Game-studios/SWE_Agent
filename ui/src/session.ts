// Turns a session's event stream into the view the timeline renders.
//
// The reducer is pure and uses structural sharing: an event only replaces the
// objects it changes, so memoised components for everything else (long
// transcripts, finished tool calls) do not re-render while text streams in.

import {
  type AgentEvent,
  type ApprovalDecision,
  type ImageRef,
  type SessionSnapshot,
  type Todo,
  type ToolDisplay,
  type ToolKind,
  type TurnOutcome,
  type Usage,
  api,
  openEventStream,
} from './api';

export const ROOT = 'root';
const MAX_LIVE_OUTPUT = 200_000;

export interface ApprovalView {
  requestId: string;
  reason: string;
  alwaysLabel?: string;
  decision?: ApprovalDecision;
}

export interface ToolView {
  id: string;
  agentId: string;
  name: string;
  kind: ToolKind | null;
  input: Record<string, unknown>;
  summary: string;
  /** streaming: arguments still being generated; waiting: not started (possibly needs approval). */
  status: 'streaming' | 'waiting' | 'running' | 'done' | 'error';
  inputChars: number;
  output: string;
  /** Output received while the tool is still running. */
  live: string;
  durationMs?: number;
  display?: ToolDisplay;
  approval?: ApprovalView;
  /** Sub-agent started by this call (`task`). */
  childAgentId?: string;
}

export type Block = { type: 'text'; text: string } | { type: 'reasoning'; text: string } | { type: 'tool'; id: string };

export type Item =
  | { kind: 'user'; key: string; text: string; images: ImageRef[] }
  | { kind: 'assistant'; key: string; blocks: Block[]; note?: string }
  | { kind: 'notice'; key: string; text: string }
  | { kind: 'error'; key: string; message: string }
  | { kind: 'compacted'; key: string; summary: string; tokensBefore: number }
  | { kind: 'turn'; key: string; outcome: TurnOutcome; usage: Usage };

export interface AgentView {
  id: string;
  parentId: string | null;
  description: string;
  agentType: string;
  provider: string;
  model: string;
  worktree?: string;
  status: 'idle' | 'running' | TurnOutcome;
  usage: Usage | null;
  items: Item[];
  streamingText: string;
  streamingReasoning: string;
  /** Tool calls whose arguments are streaming and are not part of a message yet. */
  pendingTools: string[];
  retrying: { attempt: number; message: string; delayMs: number } | null;
}

export interface SessionView {
  agents: Record<string, AgentView>;
  tools: Record<string, ToolView>;
  todos: Todo[];
  lastSeq: number;
  running: boolean;
}

function newAgent(id: string, partial: Partial<AgentView> = {}): AgentView {
  return {
    id,
    parentId: null,
    description: id === ROOT ? 'Main agent' : 'Sub-agent',
    agentType: 'general',
    provider: '',
    model: '',
    status: 'idle',
    usage: null,
    items: [],
    streamingText: '',
    streamingReasoning: '',
    pendingTools: [],
    retrying: null,
    ...partial,
  };
}

export function emptyView(): SessionView {
  return { agents: { [ROOT]: newAgent(ROOT) }, tools: {}, todos: [], lastSeq: 0, running: false };
}

function withAgent(view: SessionView, id: string, change: (agent: AgentView) => AgentView): SessionView {
  const current = view.agents[id] ?? newAgent(id);
  return { ...view, agents: { ...view.agents, [id]: change(current) } };
}

function withTool(
  view: SessionView,
  id: string,
  agentId: string,
  change: (tool: ToolView) => ToolView,
): SessionView {
  const current: ToolView = view.tools[id] ?? {
    id,
    agentId,
    name: '',
    kind: null,
    input: {},
    summary: '',
    status: 'waiting',
    inputChars: 0,
    output: '',
    live: '',
  };
  return { ...view, tools: { ...view.tools, [id]: change(current) } };
}

function pushItem(view: SessionView, agentId: string, item: Item): SessionView {
  return withAgent(view, agentId, (agent) => ({ ...agent, items: [...agent.items, item] }));
}

const clearStreaming = (agent: AgentView): AgentView =>
  agent.streamingText || agent.streamingReasoning || agent.pendingTools.length || agent.retrying
    ? { ...agent, streamingText: '', streamingReasoning: '', pendingTools: [], retrying: null }
    : agent;

/** Apply one event. Durable events that were already applied are ignored. */
export function applyEvent(view: SessionView, event: AgentEvent): SessionView {
  const transient =
    event.type === 'assistant_delta' ||
    event.type === 'reasoning_delta' ||
    event.type === 'tool_call_streaming' ||
    event.type === 'tool_output_delta' ||
    event.type === 'retrying';
  if (!transient) {
    if (event.seq <= view.lastSeq) return view;
    view = { ...view, lastSeq: event.seq };
  }
  const agentId = event.agent_id;
  const key = `${event.seq}`;

  switch (event.type) {
    case 'user_message':
      return {
        ...pushItem(view, ROOT, { kind: 'user', key, text: event.text, images: event.images ?? [] }),
        running: true,
      };
    case 'turn_started':
      return { ...withAgent(view, ROOT, (agent) => ({ ...agent, status: 'running' })), running: true };
    case 'assistant_delta':
      return withAgent(view, agentId, (agent) => ({
        ...agent,
        streamingText: agent.streamingText + event.text,
        retrying: null,
      }));
    case 'reasoning_delta':
      return withAgent(view, agentId, (agent) => ({
        ...agent,
        streamingReasoning: agent.streamingReasoning + event.text,
        retrying: null,
      }));
    case 'tool_call_streaming': {
      view = withTool(view, event.tool_call_id, agentId, (tool) =>
        tool.status === 'waiting' || tool.status === 'streaming'
          ? { ...tool, name: event.name, status: 'streaming', inputChars: event.input_chars }
          : tool,
      );
      return withAgent(view, agentId, (agent) =>
        agent.pendingTools.includes(event.tool_call_id)
          ? agent
          : { ...agent, pendingTools: [...agent.pendingTools, event.tool_call_id], retrying: null },
      );
    }
    case 'assistant_message': {
      const blocks: Block[] = [];
      for (const block of event.message.content) {
        if (block.type === 'text' && block.text.trim()) {
          blocks.push({ type: 'text', text: block.text });
        } else if (block.type === 'reasoning' && block.text.trim()) {
          blocks.push({ type: 'reasoning', text: block.text });
        } else if (block.type === 'tool_call') {
          view = withTool(view, block.id, agentId, (tool) => ({
            ...tool,
            name: block.name,
            input: block.input,
            status: tool.status === 'streaming' ? 'waiting' : tool.status,
          }));
          blocks.push({ type: 'tool', id: block.id });
        }
      }
      const stop = event.stop_reason;
      const note =
        stop === 'max_tokens'
          ? 'cut off at the output limit'
          : stop === 'refusal'
            ? 'declined by the model'
            : typeof stop === 'object' && stop.other === 'cancelled'
              ? 'stopped'
              : undefined;
      view = withAgent(view, agentId, clearStreaming);
      return blocks.length ? pushItem(view, agentId, { kind: 'assistant', key, blocks, note }) : view;
    }
    case 'tool_started':
      return withTool(view, event.tool_call_id, agentId, (tool) => ({
        ...tool,
        name: event.name,
        kind: event.kind,
        input: event.input,
        summary: event.summary,
        status: 'running',
      }));
    case 'tool_output_delta':
      return withTool(view, event.tool_call_id, agentId, (tool) => {
        const live = tool.live + event.chunk;
        return { ...tool, live: live.length > MAX_LIVE_OUTPUT ? live.slice(-MAX_LIVE_OUTPUT) : live };
      });
    case 'tool_finished':
      return withTool(view, event.tool_call_id, agentId, (tool) => ({
        ...tool,
        name: tool.name || event.name,
        status: event.is_error ? 'error' : 'done',
        output: event.output,
        live: '',
        durationMs: event.duration_ms,
        display: event.display,
      }));
    case 'approval_requested':
      return withTool(view, event.tool_call_id, agentId, (tool) => ({
        ...tool,
        name: event.name,
        kind: event.kind,
        input: event.input,
        summary: event.summary,
        status: 'waiting',
        approval: { requestId: event.request_id, reason: event.reason, alwaysLabel: event.always_label },
      }));
    case 'approval_resolved': {
      const tool = Object.values(view.tools).find((candidate) => candidate.approval?.requestId === event.request_id);
      if (!tool?.approval) return view;
      const approval = { ...tool.approval, decision: event.decision };
      return { ...view, tools: { ...view.tools, [tool.id]: { ...tool, approval } } };
    }
    case 'todos_updated':
      return { ...view, todos: event.todos };
    case 'agent_spawned': {
      view = withAgent(view, agentId, (agent) => ({
        ...agent,
        parentId: event.parent_agent_id,
        description: event.description,
        agentType: event.agent_type,
        provider: event.provider,
        model: event.model,
        worktree: event.worktree,
        status: 'running',
      }));
      return withTool(view, event.tool_call_id, event.parent_agent_id, (tool) => ({ ...tool, childAgentId: agentId }));
    }
    case 'agent_finished':
      return withAgent(view, agentId, (agent) => ({ ...clearStreaming(agent), status: event.outcome, usage: event.usage }));
    case 'compacted':
      return pushItem(view, agentId, {
        kind: 'compacted',
        key,
        summary: event.summary,
        tokensBefore: event.tokens_before,
      });
    case 'retrying':
      return withAgent(view, agentId, (agent) => ({
        ...agent,
        streamingText: '',
        streamingReasoning: '',
        pendingTools: [],
        retrying: { attempt: event.attempt, message: event.message, delayMs: event.delay_ms },
      }));
    case 'error':
      return pushItem(withAgent(view, agentId, clearStreaming), agentId, { kind: 'error', key, message: event.message });
    case 'notice':
      return pushItem(view, agentId, { kind: 'notice', key, text: event.text });
    case 'turn_finished': {
      const agents: Record<string, AgentView> = {};
      for (const [id, agent] of Object.entries(view.agents)) {
        const cleared = clearStreaming(agent);
        // A sub-agent that never reported back did not survive the turn.
        agents[id] = cleared.status === 'running' ? { ...cleared, status: id === ROOT ? event.outcome : 'cancelled' } : cleared;
      }
      // Calls that never started stay visible, but no longer look pending.
      const tools: Record<string, ToolView> = {};
      for (const [id, tool] of Object.entries(view.tools)) {
        tools[id] =
          tool.status === 'streaming' || tool.status === 'waiting' || tool.status === 'running'
            ? { ...tool, status: 'error', output: tool.output || 'Did not run.' }
            : tool;
      }
      view = { ...view, agents, tools, running: false };
      return pushItem(view, ROOT, { kind: 'turn', key, outcome: event.outcome, usage: event.usage });
    }
  }
}

export function viewFromSnapshot(snapshot: SessionSnapshot): SessionView {
  let view = emptyView();
  for (const event of snapshot.events) view = applyEvent(view, event);
  view = { ...view, todos: snapshot.todos, running: isActive(snapshot.meta.status) };
  for (const streaming of snapshot.streaming) {
    view = withAgent(view, streaming.agent_id, (agent) => ({
      ...agent,
      streamingText: streaming.text,
      streamingReasoning: streaming.reasoning,
    }));
  }
  // After a server restart the log can end mid-turn; nothing is running then.
  if (!view.running) {
    view = finishInterrupted(view);
  }
  return view;
}

function isActive(status: string): boolean {
  return status === 'running' || status === 'waiting_approval';
}

function finishInterrupted(view: SessionView): SessionView {
  const tools: Record<string, ToolView> = {};
  for (const [id, tool] of Object.entries(view.tools)) {
    tools[id] =
      tool.status === 'done' || tool.status === 'error'
        ? tool
        : { ...tool, status: 'error', output: tool.output || 'Interrupted before it finished.' };
  }
  const agents: Record<string, AgentView> = {};
  for (const [id, agent] of Object.entries(view.agents)) {
    agents[id] = agent.status === 'running' ? { ...clearStreaming(agent), status: 'cancelled' } : clearStreaming(agent);
  }
  return { ...view, tools, agents };
}

/** Tool calls of `view` that are waiting for the user's decision. */
export function pendingApprovals(view: SessionView): ToolView[] {
  return Object.values(view.tools).filter((tool) => tool.approval && !tool.approval.decision && tool.status === 'waiting');
}

type Listener = () => void;

/** Loads a session, keeps it in sync with the server, and notifies React. */
export class SessionStore {
  readonly id: string;
  view: SessionView = emptyView();
  state: 'loading' | 'ready' | 'error' = 'loading';
  error = '';
  /** Messages accepted by the server but not yet picked up by the agent. */
  queued: string[] = [];
  connected = true;

  private listeners = new Set<Listener>();
  private pending: AgentEvent[] = [];
  private flushTimer: ReturnType<typeof setTimeout> | null = null;
  private closeStream: (() => void) | null = null;
  private disposed = false;
  private wasDisconnected = false;

  constructor(id: string) {
    this.id = id;
    void this.load();
  }

  subscribe = (listener: Listener): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  /** Changes whenever anything observable changed (for useSyncExternalStore). */
  version = 0;
  getVersion = (): number => this.version;

  private notify(): void {
    this.version += 1;
    for (const listener of this.listeners) listener();
  }

  private async load(): Promise<void> {
    try {
      const snapshot = await api.session(this.id);
      if (this.disposed) return;
      this.view = viewFromSnapshot(snapshot);
      this.state = 'ready';
      this.notify();
      this.closeStream = openEventStream<AgentEvent>(
        `/sessions/${this.id}/events`,
        () => ({ after: String(this.view.lastSeq) }),
        (event) => this.enqueue(event),
        (connected) => this.onConnection(connected),
      );
    } catch (error) {
      if (this.disposed) return;
      this.state = 'error';
      this.error = error instanceof Error ? error.message : String(error);
      this.notify();
    }
  }

  private onConnection(connected: boolean): void {
    if (!connected) {
      this.wasDisconnected = true;
    } else if (this.wasDisconnected) {
      // Deltas sent while we were away are gone; take the server's buffers.
      this.wasDisconnected = false;
      void this.resync();
    }
    if (this.connected !== connected) {
      this.connected = connected;
      this.notify();
    }
  }

  private async resync(): Promise<void> {
    try {
      const snapshot = await api.session(this.id);
      if (this.disposed) return;
      this.flush();
      this.view = viewFromSnapshot(snapshot);
      this.notify();
    } catch {
      // The stream keeps retrying; the next successful reconnect resyncs.
    }
  }

  private enqueue(event: AgentEvent): void {
    this.pending.push(event);
    // Coalesce bursts (token deltas) into one render per frame.
    this.flushTimer ??= setTimeout(() => this.flush(), 16);
  }

  private flush(): void {
    if (this.flushTimer) clearTimeout(this.flushTimer);
    this.flushTimer = null;
    if (!this.pending.length) return;
    let view = this.view;
    let queued = this.queued;
    for (const event of this.pending) {
      view = applyEvent(view, event);
      if (event.type === 'user_message') {
        const index = queued.indexOf(event.text);
        if (index >= 0) queued = queued.filter((_, i) => i !== index);
      } else if (event.type === 'turn_finished' && event.outcome === 'cancelled') {
        // Stopping a turn also drops what was queued behind it.
        queued = [];
      }
    }
    this.pending = [];
    this.view = view;
    this.queued = queued;
    this.notify();
  }

  async send(text: string, images: ImageRef[]): Promise<void> {
    const { queued } = await api.sendMessage(this.id, text, images);
    if (queued) {
      this.queued = [...this.queued, text];
      this.notify();
    }
  }

  dispose(): void {
    this.disposed = true;
    this.closeStream?.();
    if (this.flushTimer) clearTimeout(this.flushTimer);
    this.listeners.clear();
  }
}
