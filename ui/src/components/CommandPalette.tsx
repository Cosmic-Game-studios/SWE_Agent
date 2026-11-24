// Command palette: jump to a session or run an action from the keyboard.

import { type KeyboardEvent, useEffect, useMemo, useRef, useState } from 'react';

import type { SessionMeta } from '../api';
import { STATUS_LABEL, baseName, relativeTime } from '../format';
import { navigate, requestDelete, useApp } from '../store';
import { StatusMark } from './Sidebar';
import { Icon, type IconName, Kbd } from './ui';

interface Item {
  id: string;
  group: 'Actions' | 'Sessions';
  label: string;
  detail?: string;
  icon?: IconName;
  session?: SessionMeta;
  /** Lowercase text the query is matched against. */
  haystack: string;
  run: () => void;
}

function matches(item: Item, query: string): boolean {
  return query
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean)
    .every((word) => item.haystack.includes(word));
}

export function CommandPalette({
  onClose,
  onNewSession,
  onOpenSettings,
}: {
  onClose: () => void;
  onNewSession: () => void;
  onOpenSettings: () => void;
}) {
  const sessions = useApp((state) => state.sessions);
  const [query, setQuery] = useState('');
  const [index, setIndex] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  const items = useMemo<Item[]>(() => {
    const actions: Item[] = [
      { id: 'new', group: 'Actions', label: 'New session', icon: 'plus', haystack: 'new session create start', run: onNewSession },
      {
        id: 'agents',
        group: 'Actions',
        label: 'All agents',
        icon: 'grid',
        haystack: 'all agents board overview',
        run: () => navigate({ page: 'agents' }),
      },
      { id: 'home', group: 'Actions', label: 'Home', icon: 'terminal', haystack: 'home start', run: () => navigate({ page: 'home' }) },
      {
        id: 'settings',
        group: 'Actions',
        label: 'Settings',
        detail: 'API keys, models, defaults',
        icon: 'settings',
        haystack: 'settings preferences api keys models providers',
        run: onOpenSettings,
      },
    ];
    const sessionItems: Item[] = sessions.map((session) => ({
      id: session.id,
      group: 'Sessions',
      label: session.title,
      detail: `${baseName(session.workspace)} · ${relativeTime(session.updated_at)}`,
      session,
      haystack: `${session.title} ${session.workspace} ${session.model} ${STATUS_LABEL[session.status]}`.toLowerCase(),
      run: () => navigate({ page: 'session', id: session.id }),
    }));
    return [...actions, ...sessionItems].filter((item) => matches(item, query));
  }, [sessions, query, onNewSession, onOpenSettings]);

  const active = Math.min(index, Math.max(items.length - 1, 0));

  useEffect(() => {
    listRef.current?.querySelector('.palette-item.active')?.scrollIntoView({ block: 'nearest' });
  }, [active, items]);

  const run = (item: Item | undefined) => {
    if (!item) return;
    onClose();
    item.run();
  };

  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      setIndex(items.length ? (active + 1) % items.length : 0);
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      setIndex(items.length ? (active - 1 + items.length) % items.length : 0);
    } else if (event.key === 'Enter') {
      event.preventDefault();
      run(items[active]);
    } else if (event.key === 'Escape') {
      event.preventDefault();
      onClose();
    } else if (event.key === 'Delete' && event.shiftKey) {
      const session = items[active]?.session;
      if (session) {
        event.preventDefault();
        onClose();
        requestDelete(session);
      }
    }
  };

  let lastGroup = '';
  return (
    <div className="overlay" onMouseDown={(event) => event.target === event.currentTarget && onClose()}>
      <div className="palette" role="dialog" aria-modal="true" aria-label="Command palette" onKeyDown={onKeyDown}>
        <div className="palette-input">
          <Icon name="search" size={16} />
          <input
            autoFocus
            value={query}
            placeholder="Search sessions and commands…"
            aria-label="Search sessions and commands"
            spellCheck={false}
            onChange={(event) => {
              setQuery(event.target.value);
              setIndex(0);
            }}
          />
          <Kbd>Esc</Kbd>
        </div>
        <div className="palette-list" ref={listRef} role="listbox">
          {items.length === 0 ? <p className="palette-empty">Nothing matches “{query}”.</p> : null}
          {items.map((item, position) => {
            const heading = item.group !== lastGroup ? item.group : null;
            lastGroup = item.group;
            return (
              <div key={item.id}>
                {heading ? <div className="palette-group">{heading}</div> : null}
                <button
                  type="button"
                  role="option"
                  aria-selected={position === active}
                  className={position === active ? 'palette-item active' : 'palette-item'}
                  onMouseMove={() => setIndex(position)}
                  onClick={() => run(item)}
                >
                  <span className="palette-icon">
                    {item.session ? <StatusMark status={item.session.status} /> : item.icon ? <Icon name={item.icon} size={14} /> : null}
                  </span>
                  <span className="palette-label">{item.label}</span>
                  {item.detail ? <span className="palette-detail">{item.detail}</span> : null}
                </button>
              </div>
            );
          })}
        </div>
        <footer className="palette-footer">
          <span>
            <Kbd>↑</Kbd>
            <Kbd>↓</Kbd> navigate
          </span>
          <span>
            <Kbd>↵</Kbd> open
          </span>
          <span>
            <Kbd>Shift</Kbd>
            <Kbd>Del</Kbd> delete session
          </span>
        </footer>
      </div>
    </div>
  );
}
