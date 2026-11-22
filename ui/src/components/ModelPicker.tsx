import { useEffect, useMemo, useState } from 'react';

import { formatTokens } from '../format';
import { loadModels, useApp } from '../store';
import { Button, Icon, Popover, Spinner } from './ui';

/** Choose a provider and one of its models (or type any model id). */
export function ModelPicker({
  provider,
  model,
  onChange,
  onOpenSettings,
  placement = 'top',
  allowInherit,
}: {
  provider: string | null;
  model: string | null;
  onChange: (provider: string | null, model: string | null) => void;
  onOpenSettings?: () => void;
  placement?: 'top' | 'bottom';
  /** Offer "same as the main agent" (for the sub-agent setting). */
  allowInherit?: boolean;
}) {
  const providers = useApp((state) => state.providers);
  const label = model ? model : allowInherit ? 'Same as main agent' : 'Choose a model';
  const providerName = providers.find((entry) => entry.id === provider)?.name ?? provider;
  return (
    <Popover
      placement={placement}
      trigger={({ toggle, open }) => (
        <button type="button" className="chip" onClick={toggle} aria-expanded={open} title="Model">
          <Icon name="bot" size={13} />
          <span className="chip-main">{label}</span>
          {model && providerName ? <span className="chip-sub">{providerName}</span> : null}
          <Icon name="chevron-down" size={12} />
        </button>
      )}
    >
      {(close) => (
        <ModelList
          provider={provider}
          model={model}
          allowInherit={allowInherit}
          onOpenSettings={onOpenSettings}
          onPick={(nextProvider, nextModel) => {
            onChange(nextProvider, nextModel);
            close();
          }}
        />
      )}
    </Popover>
  );
}

function ModelList({
  provider,
  model,
  onPick,
  onOpenSettings,
  allowInherit,
}: {
  provider: string | null;
  model: string | null;
  onPick: (provider: string | null, model: string | null) => void;
  onOpenSettings?: () => void;
  allowInherit?: boolean;
}) {
  const providers = useApp((state) => state.providers);
  const usable = providers.filter((entry) => entry.key.source !== 'missing');
  const [active, setActive] = useState<string | null>(
    provider && usable.some((entry) => entry.id === provider) ? provider : (usable[0]?.id ?? null),
  );
  const [query, setQuery] = useState('');
  const models = useApp((state) => (active ? state.models[active] : undefined));

  useEffect(() => {
    if (active) void loadModels(active);
  }, [active]);

  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase();
    const list = models?.list ?? [];
    return needle ? list.filter((entry) => `${entry.id} ${entry.name}`.toLowerCase().includes(needle)) : list;
  }, [models, query]);
  const custom = query.trim();
  const exact = filtered.some((entry) => entry.id === custom);

  if (!usable.length) {
    return (
      <div className="picker picker-empty">
        <p>No provider is set up yet.</p>
        {onOpenSettings ? (
          <Button variant="primary" small icon="key" onClick={onOpenSettings}>
            Add an API key
          </Button>
        ) : null}
      </div>
    );
  }

  return (
    <div className="picker">
      <div className="picker-providers" role="tablist" aria-label="Provider">
        {usable.map((entry) => (
          <button
            key={entry.id}
            type="button"
            role="tab"
            aria-selected={entry.id === active}
            className={entry.id === active ? 'picker-provider active' : 'picker-provider'}
            onClick={() => {
              setActive(entry.id);
              setQuery('');
            }}
          >
            {entry.name}
          </button>
        ))}
        {providers.length > usable.length && onOpenSettings ? (
          <button type="button" className="picker-provider picker-more" onClick={onOpenSettings}>
            <Icon name="plus" size={12} /> Add provider
          </button>
        ) : null}
      </div>
      <div className="picker-models">
        <div className="picker-search">
          <Icon name="search" size={13} />
          <input
            autoFocus
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="Search or type a model id"
            aria-label="Search models"
            onKeyDown={(event) => {
              if (event.key === 'Enter' && active) {
                const first = filtered[0];
                if (custom && !exact) onPick(active, custom);
                else if (first) onPick(active, first.id);
              }
            }}
          />
          {active ? (
            <button type="button" className="picker-refresh" title="Reload the model list" onClick={() => loadModels(active, true)}>
              <Icon name="refresh" size={12} />
            </button>
          ) : null}
        </div>
        <div className="picker-list" role="listbox">
          {allowInherit ? (
            <button type="button" className={!model ? 'picker-item active' : 'picker-item'} onClick={() => onPick(null, null)}>
              <span>Same as main agent</span>
            </button>
          ) : null}
          {models?.loading && !models.list.length ? (
            <div className="picker-note">
              <Spinner size={12} /> Loading models…
            </div>
          ) : null}
          {models?.error ? <div className="picker-note error">{models.error}</div> : null}
          {filtered.map((entry) => (
            <button
              key={entry.id}
              type="button"
              role="option"
              aria-selected={entry.id === model && active === provider}
              className={entry.id === model && active === provider ? 'picker-item active' : 'picker-item'}
              onClick={() => onPick(active, entry.id)}
            >
              <span className="picker-item-name">{entry.id}</span>
              {entry.context_window ? <span className="muted">{formatTokens(entry.context_window)}</span> : null}
            </button>
          ))}
          {custom && !exact && active ? (
            <button type="button" className="picker-item" onClick={() => onPick(active, custom)}>
              <span>
                Use “<span className="picker-item-name">{custom}</span>”
              </span>
            </button>
          ) : null}
          {!models?.loading && !models?.error && !filtered.length && !custom ? (
            <div className="picker-note">No models listed. Type a model id to use it anyway.</div>
          ) : null}
        </div>
      </div>
    </div>
  );
}
