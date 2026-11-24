// Types mirroring the server's JSON (crates/swe-protocol) and a small client.

export interface Usage {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  reasoning_tokens: number;
}

export type ContentBlock =
  | { type: 'text'; text: string }
  | { type: 'image'; media_type: string; data: string }
  | { type: 'reasoning'; text: string }
  | { type: 'tool_call'; id: string; name: string; input: Record<string, unknown>; invalid_json?: string }
  | { type: 'tool_result'; tool_call_id: string; content: string; is_error: boolean }
  | { type: 'opaque' };

export interface Message {
  role: 'user' | 'assistant';
  content: ContentBlock[];
}

export type SessionStatus = 'idle' | 'running' | 'waiting_approval' | 'error' | 'interrupted';
export type PermissionMode = 'plan' | 'ask' | 'auto_edit' | 'full_auto';
export type Effort = 'low' | 'medium' | 'high' | 'xhigh';
export type Engine = 'native' | 'claude-cli' | 'codex-cli';
export type ToolKind = 'read' | 'edit' | 'execute' | 'network' | 'agent' | 'meta';
export type TurnOutcome = 'completed' | 'cancelled' | 'error' | 'refusal' | 'step_limit';
export type StopReason = 'end_turn' | 'tool_use' | 'max_tokens' | 'refusal' | { other: string };
export type ApprovalDecision = 'allow' | 'allow_always' | 'deny';

export interface SessionMeta {
  id: string;
  title: string;
  workspace: string;
  engine: Engine;
  provider: string;
  model: string;
  mode: PermissionMode;
  effort: Effort;
  agent: string;
  status: SessionStatus;
  created_at: string;
  updated_at: string;
  usage: Usage;
  context_tokens: number;
  context_window?: number;
  last_seq: number;
  error?: string;
  external_session_id?: string;
}

export interface Todo {
  content: string;
  status: 'pending' | 'in_progress' | 'completed';
}

export interface ImageRef {
  media_type: string;
  data: string;
}

export type ToolDisplay =
  | { kind: 'diff'; path: string; diff: string; added: number; removed: number; created: boolean }
  | { kind: 'command'; command: string; exit_code?: number; background_id?: string };

export type EventKind =
  | { type: 'user_message'; text: string; images?: ImageRef[] }
  | { type: 'turn_started'; provider: string; model: string }
  | { type: 'assistant_delta'; text: string }
  | { type: 'reasoning_delta'; text: string }
  | { type: 'tool_call_streaming'; tool_call_id: string; name: string; input_chars: number }
  | { type: 'assistant_message'; message: Message; usage: Usage; stop_reason: StopReason }
  | { type: 'tool_started'; tool_call_id: string; name: string; kind: ToolKind; input: Record<string, unknown>; summary: string }
  | { type: 'tool_output_delta'; tool_call_id: string; chunk: string }
  | {
      type: 'tool_finished';
      tool_call_id: string;
      name: string;
      output: string;
      is_error: boolean;
      duration_ms: number;
      display?: ToolDisplay;
    }
  | {
      type: 'approval_requested';
      request_id: string;
      tool_call_id: string;
      name: string;
      kind: ToolKind;
      input: Record<string, unknown>;
      summary: string;
      reason: string;
      always_label?: string;
    }
  | { type: 'approval_resolved'; request_id: string; decision: ApprovalDecision }
  | { type: 'todos_updated'; todos: Todo[] }
  | {
      type: 'agent_spawned';
      parent_agent_id: string;
      tool_call_id: string;
      description: string;
      agent_type: string;
      provider: string;
      model: string;
      worktree?: string;
    }
  | { type: 'agent_finished'; outcome: TurnOutcome; usage: Usage }
  | { type: 'compacted'; summary: string; tokens_before: number }
  | { type: 'retrying'; attempt: number; delay_ms: number; message: string }
  | { type: 'error'; message: string }
  | { type: 'notice'; text: string }
  | { type: 'turn_finished'; outcome: TurnOutcome; usage: Usage };

/** A point the files of a session can be rewound to: the user message `seq`. */
export interface Checkpoint {
  seq: number;
  files: string[];
}

export type AgentEvent = { seq: number; ts: string; agent_id: string } & EventKind;

export interface StreamingSnapshot {
  agent_id: string;
  text: string;
  reasoning: string;
}

export interface SessionSnapshot {
  meta: SessionMeta;
  events: AgentEvent[];
  streaming: StreamingSnapshot[];
  todos: Todo[];
}

export type KeyStatus =
  | { source: 'env'; variable: string }
  | { source: 'stored' }
  | { source: 'not_needed' }
  | { source: 'missing' };

export interface ProviderInfo {
  id: string;
  name: string;
  protocol: string;
  base_url: string;
  key: KeyStatus;
  key_envs: string[];
  builtin: boolean;
}

export interface ModelInfo {
  id: string;
  name: string;
  context_window?: number;
  max_output_tokens?: number;
}

export interface EngineInfo {
  id: Engine;
  name: string;
  description: string;
  binary: string;
  installed: boolean;
  path?: string;
}

export interface ServerMeta {
  version: string;
  home: string;
  cwd: string | null;
  user_home: string | null;
  profiles: { id: string; description: string }[];
  engines: EngineInfo[];
}

export interface Config {
  default_provider: string | null;
  default_model: string | null;
  default_mode: PermissionMode;
  default_effort: Effort;
  subagent_provider: string | null;
  subagent_model: string | null;
  compact_threshold_tokens: number;
  max_steps_per_turn: number;
  max_agent_depth: number;
}

export interface McpServerStatus {
  name: string;
  running: boolean;
  tools: number;
  error?: string;
}

export interface DirListing {
  path: string;
  parent: string | null;
  is_git: boolean;
  entries: { name: string; path: string; is_git: boolean }[];
}

export interface WorkspaceDiff {
  is_git: boolean;
  branch: string | null;
  diff: string;
  truncated: boolean;
}

export type GlobalEvent = { type: 'session_updated'; session: SessionMeta } | { type: 'session_deleted'; id: string };

export interface CreateSessionRequest {
  workspace: string;
  title?: string;
  engine?: Engine;
  provider?: string;
  model?: string;
  mode?: PermissionMode;
  effort?: Effort;
  agent?: string;
}

// When the server is started with --token, the UI is opened with ?token=...
const token = new URLSearchParams(window.location.search).get('token');

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  if (token) headers['Authorization'] = `Bearer ${token}`;
  let response: Response;
  try {
    response = await fetch(`/api${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch {
    throw new ApiError(0, 'Cannot reach the SWE Agent server. Is it still running?');
  }
  const text = await response.text();
  let data: unknown = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = null;
  }
  if (!response.ok) {
    const message =
      data && typeof data === 'object' && 'error' in data ? String((data as { error: unknown }).error) : `HTTP ${response.status}`;
    throw new ApiError(response.status, message);
  }
  return data as T;
}

export const api = {
  meta: () => request<ServerMeta>('GET', '/meta'),
  config: () => request<Config>('GET', '/config'),
  patchConfig: (patch: Partial<Config>) => request<Config>('PATCH', '/config', patch),
  providers: () => request<ProviderInfo[]>('GET', '/providers'),
  setKey: (provider: string, key: string | null) =>
    request<ProviderInfo>('PUT', `/providers/${encodeURIComponent(provider)}/key`, { key }),
  models: (provider: string, refresh = false) =>
    request<ModelInfo[]>('GET', `/providers/${encodeURIComponent(provider)}/models${refresh ? '?refresh=true' : ''}`),
  mcp: () => request<McpServerStatus[]>('GET', '/mcp'),
  listDir: (path?: string) => request<DirListing>('GET', `/fs/list${path ? `?path=${encodeURIComponent(path)}` : ''}`),
  sessions: () => request<SessionMeta[]>('GET', '/sessions'),
  createSession: (body: CreateSessionRequest) => request<SessionMeta>('POST', '/sessions', body),
  session: (id: string) => request<SessionSnapshot>('GET', `/sessions/${id}`),
  updateSession: (
    id: string,
    patch: Partial<Pick<SessionMeta, 'title' | 'provider' | 'model' | 'mode' | 'effort'>>,
  ) => request<SessionMeta>('PATCH', `/sessions/${id}`, patch),
  deleteSession: (id: string) => request<{ ok: boolean }>('DELETE', `/sessions/${id}`),
  sendMessage: (id: string, text: string, images: ImageRef[] = []) =>
    request<{ queued: boolean }>('POST', `/sessions/${id}/messages`, { text, images }),
  cancel: (id: string) => request<{ ok: boolean }>('POST', `/sessions/${id}/cancel`, {}),
  checkpoints: (id: string) => request<Checkpoint[]>('GET', `/sessions/${id}/checkpoints`),
  rewind: (id: string, seq: number) => request<{ restored: string[] }>('POST', `/sessions/${id}/rewind`, { seq }),
  resolveApproval: (id: string, requestId: string, decision: ApprovalDecision, feedback?: string) =>
    request<{ ok: boolean }>('POST', `/sessions/${id}/approvals/${requestId}`, { decision, feedback }),
  diff: (id: string) => request<WorkspaceDiff>('GET', `/sessions/${id}/diff`),
};

/**
 * Subscribe to a Server-Sent Events endpoint. `position` supplies the query
 * string for (re)connects, so a stream that fell behind resumes from the last
 * event the caller has actually processed.
 */
export function openEventStream<T>(
  path: string,
  position: () => Record<string, string>,
  onEvent: (event: T) => void,
  onStatus?: (connected: boolean) => void,
): () => void {
  let source: EventSource | null = null;
  let closed = false;
  let retry: ReturnType<typeof setTimeout> | undefined;

  const connect = () => {
    if (closed) return;
    const params = new URLSearchParams(position());
    if (token) params.set('token', token);
    const query = params.toString();
    source = new EventSource(`/api${path}${query ? `?${query}` : ''}`);
    source.onopen = () => onStatus?.(true);
    source.onmessage = (message) => {
      try {
        onEvent(JSON.parse(message.data) as T);
      } catch (error) {
        console.error('bad event', error);
      }
    };
    const reconnect = () => {
      // Reconnect ourselves instead of relying on the browser: the resume
      // position has to come from `position()`, not from a stale URL.
      source?.close();
      onStatus?.(false);
      clearTimeout(retry);
      retry = setTimeout(connect, 1000);
    };
    source.addEventListener('resync', reconnect);
    source.onerror = reconnect;
  };
  connect();

  return () => {
    closed = true;
    clearTimeout(retry);
    source?.close();
  };
}
