// Application-wide state: sessions, providers, settings and the current route.

import { useSyncExternalStore } from 'react';

import {
  type Config,
  type CreateSessionRequest,
  type GlobalEvent,
  type ModelInfo,
  type ProviderInfo,
  type ServerMeta,
  type SessionMeta,
  api,
  openEventStream,
} from './api';

export interface ModelsState {
  loading: boolean;
  error: string | null;
  list: ModelInfo[];
}

export type Route = { page: 'home' } | { page: 'agents' } | { page: 'session'; id: string };

export interface AppState {
  ready: boolean;
  fatal: string | null;
  meta: ServerMeta | null;
  config: Config | null;
  providers: ProviderInfo[];
  sessions: SessionMeta[];
  models: Record<string, ModelsState>;
  connected: boolean;
  route: Route;
  toast: { id: number; text: string; kind: 'error' | 'info' } | null;
}

function parseRoute(): Route {
  const hash = window.location.hash.replace(/^#\/?/, '');
  if (hash.startsWith('s/')) return { page: 'session', id: hash.slice(2) };
  if (hash === 'agents') return { page: 'agents' };
  return { page: 'home' };
}

let state: AppState = {
  ready: false,
  fatal: null,
  meta: null,
  config: null,
  providers: [],
  sessions: [],
  models: {},
  connected: true,
  route: parseRoute(),
  toast: null,
};

const listeners = new Set<() => void>();

function set(patch: Partial<AppState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

const subscribe = (listener: () => void): (() => void) => {
  listeners.add(listener);
  return () => listeners.delete(listener);
};

/** Read part of the app state. The selector must return a stable reference. */
export function useApp<T>(selector: (state: AppState) => T): T {
  return useSyncExternalStore(subscribe, () => selector(state));
}

export const getApp = (): AppState => state;

export function navigate(route: Route): void {
  const hash = route.page === 'session' ? `#/s/${route.id}` : route.page === 'agents' ? '#/agents' : '#/';
  if (window.location.hash !== hash) window.location.hash = hash;
}

window.addEventListener('hashchange', () => set({ route: parseRoute() }));

let toastId = 0;
let toastTimer: ReturnType<typeof setTimeout> | undefined;

export function toast(text: string, kind: 'error' | 'info' = 'error'): void {
  toastId += 1;
  set({ toast: { id: toastId, text, kind } });
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => set({ toast: null }), kind === 'error' ? 7000 : 3500);
}

/** Run an action and surface its failure instead of losing it. */
export async function attempt<T>(action: () => Promise<T>): Promise<T | undefined> {
  try {
    return await action();
  } catch (error) {
    toast(error instanceof Error ? error.message : String(error));
    return undefined;
  }
}

function upsertSession(session: SessionMeta): void {
  const exists = state.sessions.some((existing) => existing.id === session.id);
  const sessions = exists
    ? state.sessions.map((existing) => (existing.id === session.id ? session : existing))
    : [session, ...state.sessions];
  sessions.sort((a, b) => b.updated_at.localeCompare(a.updated_at));
  set({ sessions });
}

export async function init(): Promise<void> {
  try {
    const [meta, config, providers, sessions] = await Promise.all([
      api.meta(),
      api.config(),
      api.providers(),
      api.sessions(),
    ]);
    set({ ready: true, meta, config, providers, sessions });
  } catch (error) {
    set({ fatal: error instanceof Error ? error.message : String(error) });
    return;
  }
  let wasDown = false;
  openEventStream<GlobalEvent>(
    '/events',
    () => ({}),
    (event) => {
      if (event.type === 'session_updated') {
        upsertSession(event.session);
      } else {
        set({ sessions: state.sessions.filter((session) => session.id !== event.id) });
        if (state.route.page === 'session' && state.route.id === event.id) navigate({ page: 'home' });
      }
    },
    (connected) => {
      if (!connected) wasDown = true;
      // Updates sent while disconnected are not replayed; fetch the list again.
      if (connected && wasDown) {
        wasDown = false;
        void api.sessions().then((sessions) => set({ sessions })).catch(() => undefined);
      }
      if (state.connected !== connected) set({ connected });
    },
  );
}

export async function refreshProviders(): Promise<void> {
  set({ providers: await api.providers() });
}

export async function loadModels(provider: string, refresh = false): Promise<void> {
  const current = state.models[provider];
  if (current && !refresh && (current.loading || current.list.length)) return;
  set({ models: { ...state.models, [provider]: { loading: true, error: null, list: current?.list ?? [] } } });
  try {
    const list = await api.models(provider, refresh);
    set({ models: { ...state.models, [provider]: { loading: false, error: null, list } } });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    set({ models: { ...state.models, [provider]: { loading: false, error: message, list: [] } } });
  }
}

export async function setKey(provider: string, key: string | null): Promise<void> {
  await api.setKey(provider, key);
  const models = { ...state.models };
  delete models[provider];
  set({ models });
  await refreshProviders();
}

export async function patchConfig(patch: Partial<Config>): Promise<void> {
  set({ config: await api.patchConfig(patch) });
}

export async function createSession(request: CreateSessionRequest): Promise<SessionMeta> {
  const session = await api.createSession(request);
  upsertSession(session);
  return session;
}

export async function updateSession(
  id: string,
  patch: Partial<Pick<SessionMeta, 'title' | 'provider' | 'model' | 'mode' | 'effort'>>,
): Promise<void> {
  upsertSession(await api.updateSession(id, patch));
}

export async function deleteSession(id: string): Promise<void> {
  await api.deleteSession(id);
  set({ sessions: state.sessions.filter((session) => session.id !== id) });
  if (state.route.page === 'session' && state.route.id === id) navigate({ page: 'home' });
}

/** Providers that can be used right now (key present or not needed). */
export function usableProviders(providers: ProviderInfo[]): ProviderInfo[] {
  return providers.filter((provider) => provider.key.source !== 'missing');
}
