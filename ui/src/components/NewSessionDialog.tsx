import { useEffect, useMemo, useState } from 'react';

import { type DirListing, type Effort, type Engine, type PermissionMode, api } from '../api';
import { EFFORTS, MODES, baseName } from '../format';
import { attempt, createSession, navigate, toast, useApp } from '../store';
import { ModelPicker } from './ModelPicker';
import { Button, Dialog, Icon, Segmented, Spinner } from './ui';

function DirectoryBrowser({ start, onPick }: { start: string; onPick: (path: string) => void }) {
  const [listing, setListing] = useState<DirListing | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const open = async (path?: string) => {
    setLoading(true);
    try {
      setListing(await api.listDir(path));
      setError(null);
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void open(start || undefined);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div className="browser">
      <div className="browser-bar">
        <Button
          variant="ghost"
          small
          icon="arrow-up"
          disabled={!listing?.parent}
          onClick={() => listing?.parent && open(listing.parent)}
          aria-label="Parent directory"
          title="Parent directory"
        />
        <span className="browser-path" title={listing?.path}>
          {listing?.path ?? '…'}
        </span>
        {loading ? <Spinner size={12} /> : null}
        <Button variant="primary" small disabled={!listing} onClick={() => listing && onPick(listing.path)}>
          Use this folder
        </Button>
      </div>
      {error ? <div className="browser-note error">{error}</div> : null}
      <div className="browser-list">
        {listing?.entries.map((entry) => (
          <button key={entry.path} type="button" className="browser-entry" onDoubleClick={() => onPick(entry.path)} onClick={() => open(entry.path)}>
            <Icon name="folder" size={13} />
            <span>{entry.name}</span>
            {entry.is_git ? <Icon name="branch" size={12} className="muted" /> : null}
          </button>
        ))}
        {listing && !listing.entries.length ? <div className="browser-note">No sub-folders.</div> : null}
      </div>
    </div>
  );
}

export function NewSessionDialog({
  initialWorkspace,
  onClose,
  onOpenSettings,
}: {
  initialWorkspace?: string;
  onClose: () => void;
  onOpenSettings: () => void;
}) {
  const sessions = useApp((state) => state.sessions);
  const config = useApp((state) => state.config);
  const meta = useApp((state) => state.meta);
  const providers = useApp((state) => state.providers);

  const recent = useMemo(() => {
    const seen: string[] = [];
    for (const session of sessions) if (!seen.includes(session.workspace)) seen.push(session.workspace);
    if (meta?.cwd && !seen.includes(meta.cwd)) seen.push(meta.cwd);
    return seen.slice(0, 6);
  }, [sessions, meta]);

  const usable = providers.filter((provider) => provider.key.source !== 'missing');
  const defaultProvider =
    (config?.default_provider && usable.some((provider) => provider.id === config.default_provider) ? config.default_provider : null) ??
    sessions.find((session) => session.engine === 'native' && usable.some((provider) => provider.id === session.provider))?.provider ??
    usable[0]?.id ??
    null;
  const defaultModel =
    (defaultProvider === config?.default_provider ? config?.default_model : null) ??
    sessions.find((session) => session.engine === 'native' && session.provider === defaultProvider)?.model ??
    null;

  const [workspace, setWorkspace] = useState(initialWorkspace ?? recent[0] ?? '');
  const [browsing, setBrowsing] = useState(false);
  const [engine, setEngine] = useState<Engine>('native');
  const [provider, setProvider] = useState<string | null>(defaultProvider);
  const [model, setModel] = useState<string | null>(defaultModel);
  const [cliModel, setCliModel] = useState('');
  const [agent, setAgent] = useState('coder');
  const [mode, setMode] = useState<PermissionMode>(config?.default_mode ?? 'ask');
  const [effort, setEffort] = useState<Effort>(config?.default_effort ?? 'high');
  const [prompt, setPrompt] = useState('');
  const [creating, setCreating] = useState(false);

  const engines = meta?.engines ?? [];
  const engineInfo = engines.find((entry) => entry.id === engine);
  const native = engine === 'native';
  const ready = workspace.trim() !== '' && (native ? Boolean(provider && model) : Boolean(engineInfo?.installed));

  const create = async () => {
    setCreating(true);
    try {
      const session = await createSession({
        workspace: workspace.trim(),
        engine,
        provider: native ? (provider ?? undefined) : undefined,
        model: native ? (model ?? undefined) : cliModel.trim() || undefined,
        agent: native ? agent : undefined,
        mode,
        effort,
      });
      if (prompt.trim()) await attempt(() => api.sendMessage(session.id, prompt.trim()));
      navigate({ page: 'session', id: session.id });
      onClose();
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error));
    } finally {
      setCreating(false);
    }
  };

  return (
    <Dialog
      title="New session"
      onClose={onClose}
      wide
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={!ready} busy={creating} onClick={create}>
            {prompt.trim() ? 'Start' : 'Create session'}
          </Button>
        </>
      }
    >
      <div className="form">
        <label className="field">
          <span className="field-label">Workspace</span>
          <div className="field-row">
            <input
              value={workspace}
              onChange={(event) => setWorkspace(event.target.value)}
              placeholder="/path/to/your/project"
              spellCheck={false}
              aria-label="Workspace directory"
            />
            <Button icon="folder" onClick={() => setBrowsing(!browsing)} aria-expanded={browsing}>
              Browse
            </Button>
          </div>
          <span className="field-hint">The folder the agent reads, edits and runs commands in.</span>
        </label>
        {browsing ? (
          <DirectoryBrowser
            start={workspace || meta?.user_home || ''}
            onPick={(path) => {
              setWorkspace(path);
              setBrowsing(false);
            }}
          />
        ) : recent.length > 1 || (recent.length === 1 && recent[0] !== workspace) ? (
          <div className="recent">
            {recent.map((path) => (
              <button key={path} type="button" className={path === workspace ? 'recent-item active' : 'recent-item'} title={path} onClick={() => setWorkspace(path)}>
                <Icon name="folder" size={12} /> {baseName(path)}
              </button>
            ))}
          </div>
        ) : null}

        <div className="field">
          <span className="field-label">Engine</span>
          <Segmented<Engine>
            label="Engine"
            value={engine}
            onChange={setEngine}
            options={engines.map((entry) => ({ value: entry.id, label: entry.name, title: entry.description }))}
          />
          <span className="field-hint">{engineInfo?.description}</span>
          {!native && engineInfo && !engineInfo.installed ? (
            <div className="callout callout-warn">
              <Icon name="alert" size={14} />
              <span>
                <code>{engineInfo.binary}</code> was not found on this machine. Install the vendor's command line tool and log in with it
                first.
              </span>
            </div>
          ) : null}
          {!native && engineInfo?.installed ? (
            <div className="callout">
              <Icon name="info" size={14} />
              <span>
                This runs the vendor's own tool with the login it already has; SWE Agent never sees those credentials. Whether your plan
                covers use through another program is up to the vendor's terms, so check them. Approvals, sub-agent orchestration and
                model switching are only available with the built-in engine.
              </span>
            </div>
          ) : null}
        </div>

        {native ? (
          <div className="field">
            <span className="field-label">Model</span>
            {usable.length ? (
              <div>
                <ModelPicker
                  provider={provider}
                  model={model}
                  placement="bottom"
                  onOpenSettings={onOpenSettings}
                  onChange={(nextProvider, nextModel) => {
                    setProvider(nextProvider);
                    setModel(nextModel);
                  }}
                />
              </div>
            ) : (
              <div className="callout callout-warn">
                <Icon name="key" size={14} />
                <span>No provider has an API key yet.</span>
                <Button small variant="primary" onClick={onOpenSettings}>
                  Add a key
                </Button>
              </div>
            )}
          </div>
        ) : (
          <label className="field">
            <span className="field-label">Model (optional)</span>
            <input value={cliModel} onChange={(event) => setCliModel(event.target.value)} placeholder="Leave empty for the tool's default" spellCheck={false} />
          </label>
        )}

        {native ? (
          <div className="field">
            <span className="field-label">Agent</span>
            <Segmented
              label="Agent profile"
              value={agent}
              onChange={setAgent}
              options={(meta?.profiles ?? []).map((profile) => ({
                value: profile.id,
                label: profile.id === 'coder' ? 'Coder' : 'Orchestrator',
                title: profile.description,
              }))}
            />
            <span className="field-hint">{meta?.profiles.find((profile) => profile.id === agent)?.description}</span>
          </div>
        ) : null}

        <div className="field-columns">
          <div className="field">
            <span className="field-label">Permissions</span>
            <Segmented<PermissionMode> label="Permission mode" value={mode} onChange={setMode} options={MODES} />
            <span className="field-hint">{MODES.find((entry) => entry.value === mode)?.title}</span>
          </div>
          <div className="field">
            <span className="field-label">Reasoning effort</span>
            <Segmented<Effort> label="Reasoning effort" value={effort} onChange={setEffort} options={EFFORTS} />
            <span className="field-hint">{EFFORTS.find((entry) => entry.value === effort)?.title}</span>
          </div>
        </div>

        <label className="field">
          <span className="field-label">First message (optional)</span>
          <textarea
            value={prompt}
            rows={3}
            onChange={(event) => setPrompt(event.target.value)}
            placeholder="Describe what to build, fix or change…"
            onKeyDown={(event) => {
              if (event.key === 'Enter' && (event.metaKey || event.ctrlKey) && ready) void create();
            }}
          />
        </label>
      </div>
    </Dialog>
  );
}
