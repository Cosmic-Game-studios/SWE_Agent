// Formatting helpers and the labels used across the UI.

import type { Effort, PermissionMode, SessionStatus, TurnOutcome, Usage } from './api';

export function formatTokens(count: number): string {
  if (count >= 1_000_000) return `${(count / 1_000_000).toFixed(count >= 10_000_000 ? 0 : 1)}M`;
  if (count >= 10_000) return `${Math.round(count / 1000)}k`;
  if (count >= 1000) return `${(count / 1000).toFixed(1)}k`;
  return String(count);
}

export function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds.toFixed(seconds < 10 ? 1 : 0)}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${Math.round(seconds % 60)}s`;
}

export function relativeTime(iso: string): string {
  const seconds = Math.max(0, (Date.now() - new Date(iso).getTime()) / 1000);
  if (seconds < 45) return 'just now';
  if (seconds < 3600) return `${Math.round(seconds / 60)}m ago`;
  if (seconds < 86_400) return `${Math.round(seconds / 3600)}h ago`;
  if (seconds < 86_400 * 14) return `${Math.round(seconds / 86_400)}d ago`;
  return new Date(iso).toLocaleDateString();
}

export function usageSummary(usage: Usage): string {
  const parts = [`${formatTokens(usage.input_tokens)} in`, `${formatTokens(usage.output_tokens)} out`];
  if (usage.cache_read_tokens > 0) parts.push(`${formatTokens(usage.cache_read_tokens)} cached`);
  return parts.join(' · ');
}

/** Last path segment, for showing a workspace compactly. */
export function baseName(path: string): string {
  const parts = path.replace(/[\\/]+$/, '').split(/[\\/]/);
  return parts[parts.length - 1] || path;
}

export const MODES: { value: PermissionMode; label: string; title: string }[] = [
  { value: 'plan', label: 'Plan', title: 'Read-only: explores and proposes a plan. No edits; commands ask first.' },
  { value: 'ask', label: 'Ask', title: 'Asks before every edit and command.' },
  { value: 'auto_edit', label: 'Auto-edit', title: 'Edits files in the workspace on its own; commands still ask.' },
  { value: 'full_auto', label: 'Full auto', title: 'Never asks. Use it in a sandbox or on work you can throw away.' },
];

export const EFFORTS: { value: Effort; label: string; title: string }[] = [
  { value: 'low', label: 'Low', title: 'Fastest and cheapest; for simple, well-defined tasks.' },
  { value: 'medium', label: 'Medium', title: 'A balance of speed and depth.' },
  { value: 'high', label: 'High', title: 'Thorough reasoning; the default for real engineering work.' },
  { value: 'xhigh', label: 'Extra high', title: 'Maximum depth where the model supports it. Slower and more expensive.' },
];

export const modeLabel = (mode: PermissionMode): string => MODES.find((entry) => entry.value === mode)?.label ?? mode;

export const STATUS_LABEL: Record<SessionStatus, string> = {
  idle: 'Idle',
  running: 'Working',
  waiting_approval: 'Needs approval',
  error: 'Failed',
  interrupted: 'Interrupted',
};

export const OUTCOME_LABEL: Record<TurnOutcome, string> = {
  completed: 'Done',
  cancelled: 'Stopped',
  error: 'Failed',
  refusal: 'Declined by the model',
  step_limit: 'Step limit reached',
};

/** Label of the platform's shortcut modifier key. */
export const MOD_KEY = /Mac|iPhone|iPad/.test(navigator.platform) ? '⌘' : 'Ctrl';
