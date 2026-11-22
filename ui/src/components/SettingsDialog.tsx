import { useEffect, useState } from 'react';

import { type McpServerStatus, type ProviderInfo, api } from '../api';
import { EFFORTS, MODES, formatTokens } from '../format';
import { attempt, loadModels, patchConfig, setKey, useApp } from '../store';
import { ModelPicker } from './ModelPicker';
import { Button, Dialog, Icon, Segmented, Spinner } from './ui';

function KeyStatusTag({ provider }: { provider: ProviderInfo }) {
  const key = provider.key;
  if (key.source === 'env') return <span className="tag tag-green">from ${key.variable}</span>;
  if (key.source === 'stored') return <span className="tag tag-green">key saved</span>;
  if (key.source === 'not_needed') return <span className="tag">no key needed</span>;
  return <span className="tag tag-yellow">no key</span>;
}

function ProviderRow({ provider }: { provider: ProviderInfo }) {
  const [value, setValue] = useState('');
  const [busy, setBusy] = useState<'save' | 'remove' | 'test' | null>(null);
  const models = useApp((state) => state.models[provider.id]);
  const [tested, setTested] = useState(false);
  const usable = provider.key.source !== 'missing';

  const save = async () => {
    if (!value.trim()) return;
    setBusy('save');
    await attempt(() => setKey(provider.id, value));
    setValue('');
    setBusy(null);
    // Prove the key works right away instead of failing in the first session.
    setTested(true);
    void loadModels(provider.id, true);
  };

  return (
    <div className="provider">
      <div className="provider-head">
        <div>
          <span className="provider-name">{provider.name}</span>
          <span className="provider-detail">{provider.base_url || provider.protocol}</span>
        </div>
        <KeyStatusTag provider={provider} />
      </div>
      {provider.key.source !== 'not_needed' ? (
        <form
          className="field-row"
          onSubmit={(event) => {
            event.preventDefault();
            void save();
          }}
        >
          <input
            type="password"
            autoComplete="off"
            value={value}
            onChange={(event) => setValue(event.target.value)}
            placeholder={provider.key.source === 'env' ? `Set in the environment (${provider.key.variable})` : usable ? 'Replace the saved key' : 'Paste an API key'}
            aria-label={`API key for ${provider.name}`}
          />
          <Button type="submit" variant="primary" disabled={!value.trim()} busy={busy === 'save'}>
            Save
          </Button>
          {provider.key.source === 'stored' ? (
            <Button
              variant="ghost"
              busy={busy === 'remove'}
              onClick={async () => {
                setBusy('remove');
                await attempt(() => setKey(provider.id, null));
                setBusy(null);
                setTested(false);
              }}
            >
              Remove
            </Button>
          ) : null}
        </form>
      ) : null}
      {usable ? (
        <div className="provider-test">
          <Button
            small
            variant="ghost"
            icon="refresh"
            busy={models?.loading}
            onClick={() => {
              setTested(true);
              void loadModels(provider.id, true);
            }}
          >
            Test connection
          </Button>
          {tested && models && !models.loading ? (
            models.error ? (
              <span className="error">{models.error}</span>
            ) : (
              <span className="ok">
                <Icon name="check" size={12} /> Works: {models.list.length} model{models.list.length === 1 ? '' : 's'} available
              </span>
            )
          ) : null}
        </div>
      ) : provider.key_envs.length ? (
        <p className="field-hint">
          Or start the server with <code>{provider.key_envs[0]}</code> set; then nothing is stored on disk.
        </p>
      ) : null}
    </div>
  );
}

function McpSection() {
  const [servers, setServers] = useState<McpServerStatus[] | null>(null);
  useEffect(() => {
    void attempt(api.mcp).then((result) => setServers(result ?? []));
  }, []);
  if (!servers) {
    return (
      <p className="field-hint">
        <Spinner size={11} /> Starting MCP servers…
      </p>
    );
  }
  if (!servers.length) {
    return (
      <p className="field-hint">
        None configured. Add servers under <code>[mcp_servers.&lt;name&gt;]</code> in <code>config.toml</code> to give the agent more tools
        (issue trackers, databases, browsers, …).
      </p>
    );
  }
  return (
    <ul className="plain-list">
      {servers.map((server) => (
        <li key={server.name}>
          <span className={`dot dot-${server.running ? 'completed' : 'error'}`} />
          <span className="provider-name">{server.name}</span>
          <span className="muted">{server.running ? `${server.tools} tools` : server.error}</span>
        </li>
      ))}
    </ul>
  );
}

export function SettingsDialog({ onClose }: { onClose: () => void }) {
  const providers = useApp((state) => state.providers);
  const config = useApp((state) => state.config);
  const meta = useApp((state) => state.meta);
  const [threshold, setThreshold] = useState(String(config?.compact_threshold_tokens ?? 200000));

  if (!config || !meta) return null;
  const save = (patch: Parameters<typeof patchConfig>[0]) => void attempt(() => patchConfig(patch));

  return (
    <Dialog title="Settings" onClose={onClose} wide>
      <section className="settings-section">
        <h3>Providers</h3>
        <p className="section-hint">
          Bring your own API key. Keys are stored only on this machine (<code>{meta.home}/credentials.json</code>, readable by you alone)
          and are sent to nobody but the provider itself.
        </p>
        {providers.map((provider) => (
          <ProviderRow key={provider.id} provider={provider} />
        ))}
        <p className="field-hint">
          Other OpenAI-compatible endpoints (Ollama, LM Studio, vLLM, Groq, DeepSeek, …) can be added under{' '}
          <code>[providers.&lt;name&gt;]</code> in <code>{meta.home}/config.toml</code>.
        </p>
      </section>

      <section className="settings-section">
        <h3>Defaults for new sessions</h3>
        <div className="form">
          <div className="field">
            <span className="field-label">Model</span>
            <div>
              <ModelPicker
                provider={config.default_provider}
                model={config.default_model}
                placement="bottom"
                onChange={(provider, model) => save({ default_provider: provider, default_model: model })}
              />
            </div>
          </div>
          <div className="field-columns">
            <div className="field">
              <span className="field-label">Permissions</span>
              <Segmented label="Default permission mode" value={config.default_mode} options={MODES} onChange={(mode) => save({ default_mode: mode })} />
              <span className="field-hint">{MODES.find((entry) => entry.value === config.default_mode)?.title}</span>
            </div>
            <div className="field">
              <span className="field-label">Reasoning effort</span>
              <Segmented label="Default reasoning effort" value={config.default_effort} options={EFFORTS} onChange={(effort) => save({ default_effort: effort })} />
            </div>
          </div>
        </div>
      </section>

      <section className="settings-section">
        <h3>Sub-agents and long runs</h3>
        <div className="form">
          <div className="field">
            <span className="field-label">Sub-agent model</span>
            <div>
              <ModelPicker
                allowInherit
                provider={config.subagent_provider}
                model={config.subagent_model}
                placement="bottom"
                onChange={(provider, model) => save({ subagent_provider: provider, subagent_model: model })}
              />
            </div>
            <span className="field-hint">A faster, cheaper model for delegated work keeps orchestrated runs affordable.</span>
          </div>
          <label className="field">
            <span className="field-label">Summarise the conversation above</span>
            <div className="field-row">
              <input
                type="number"
                min={8000}
                step={10000}
                value={threshold}
                onChange={(event) => setThreshold(event.target.value)}
                onBlur={() => {
                  const value = Number(threshold);
                  if (Number.isFinite(value) && value >= 8000 && value !== config.compact_threshold_tokens) {
                    save({ compact_threshold_tokens: Math.round(value) });
                  } else {
                    setThreshold(String(config.compact_threshold_tokens));
                  }
                }}
                aria-label="Compaction threshold in tokens"
              />
              <span className="muted">tokens ({formatTokens(config.compact_threshold_tokens)})</span>
            </div>
            <span className="field-hint">
              Past this context size (or 80% of the model's window, whichever is smaller) the history is replaced by a summary, so a
              session can keep working for hours.
            </span>
          </label>
        </div>
      </section>

      <section className="settings-section">
        <h3>Vendor CLI engines</h3>
        <p className="section-hint">
          Optional: run a session on a vendor's own command line tool with the login it already has, instead of an API key. SWE Agent
          only starts the tool and never touches its credentials. Whether a plan covers use through another program is defined by the
          vendor; Google's terms exclude it for consumer plans, which is why Gemini is available with an API key only.
        </p>
        <ul className="plain-list">
          {meta.engines
            .filter((engine) => engine.id !== 'native')
            .map((engine) => (
              <li key={engine.id}>
                <span className={`dot dot-${engine.installed ? 'completed' : 'idle'}`} />
                <span className="provider-name">{engine.binary}</span>
                <span className="muted">{engine.installed ? engine.path : 'not installed'}</span>
              </li>
            ))}
        </ul>
      </section>

      <section className="settings-section">
        <h3>MCP servers</h3>
        <McpSection />
      </section>

      <section className="settings-section">
        <h3>About</h3>
        <p className="field-hint">
          SWE Agent {meta.version}. Sessions, settings and keys live in <code>{meta.home}</code>.
        </p>
      </section>
    </Dialog>
  );
}
