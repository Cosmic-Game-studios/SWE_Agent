// Small building blocks: icons, buttons, dialog, popover.

import { type ButtonHTMLAttributes, type ReactNode, useEffect, useLayoutEffect, useRef, useState } from 'react';

const ICONS = {
  plus: 'M12 5v14M5 12h14',
  'arrow-up': 'M12 19V5M5 12l7-7 7 7',
  'arrow-right': 'M5 12h14M12 5l7 7-7 7',
  'chevron-right': 'M9 18l6-6-6-6',
  'chevron-down': 'M6 9l6 6 6-6',
  check: 'M20 6L9 17l-5-5',
  x: 'M18 6L6 18M6 6l12 12',
  terminal: 'M4 17l6-6-6-6M12 19h8',
  file: 'M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8zM14 2v6h6',
  edit: 'M12 20h9M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z',
  search: 'M19 11a8 8 0 1 1-16 0 8 8 0 0 1 16 0zM21 21l-4.35-4.35',
  globe:
    'M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0zM2 12h20M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z',
  settings: 'M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6',
  folder: 'M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z',
  branch: 'M6 3v12M21 6a3 3 0 1 1-6 0 3 3 0 0 1 6 0zM9 18a3 3 0 1 1-6 0 3 3 0 0 1 6 0zM18 9a9 9 0 0 1-9 9',
  alert:
    'M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0zM12 9v4M12 17h.01',
  info: 'M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0zM12 16v-4M12 8h.01',
  trash: 'M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2',
  list: 'M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01',
  layers: 'M12 2L2 7l10 5 10-5-10-5zM2 17l10 5 10-5M2 12l10 5 10-5',
  refresh: 'M23 4v6h-6M1 20v-6h6M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15',
  key: 'M21 2l-2 2m-7.61 7.61a5.5 5.5 0 1 1-7.778 7.778 5.5 5.5 0 0 1 7.777-7.777zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4',
  copy: 'M20 9h-9a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h9a2 2 0 0 0 2-2v-9a2 2 0 0 0-2-2zM5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1',
  image: 'M19 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V5a2 2 0 0 0-2-2zM8.5 10a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3zM21 15l-5-5L5 21',
  panel: 'M19 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V5a2 2 0 0 0-2-2zM15 3v18',
  bot: 'M18 4H6a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2zM9 9h6v6H9zM9 1v3M15 1v3M9 20v3M15 20v3M20 9h3M20 14h3M1 9h3M1 14h3',
  zap: 'M13 2L3 14h9l-1 8 10-12h-9l1-8z',
  grid: 'M3 3h7v7H3zM14 3h7v7h-7zM14 14h7v7h-7zM3 14h7v7H3z',
  shield: 'M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z',
  clock: 'M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0zM12 6v6l4 2',
  archive: 'M21 8v13H3V8M1 3h22v5H1zM10 12h4',
} as const;

export type IconName = keyof typeof ICONS;

export function Icon({ name, size = 16, className }: { name: IconName; size?: number; className?: string }) {
  return (
    <svg
      className={className ? `icon ${className}` : 'icon'}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d={ICONS[name]} />
    </svg>
  );
}

export function Kbd({ children }: { children: ReactNode }) {
  return <kbd className="kbd">{children}</kbd>;
}

/** Copies text to the clipboard and confirms it with a check mark for a moment. */
export function CopyButton({ text, label = 'Copy', className }: { text: string | (() => string); label?: string; className?: string }) {
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), 1500);
    return () => clearTimeout(timer);
  }, [copied]);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(typeof text === 'function' ? text() : text);
      setCopied(true);
    } catch {
      // Clipboard access can be denied (insecure origin, permissions); leave the icon unchanged.
    }
  };
  return (
    <button
      type="button"
      className={className ? `copy-button ${className}` : 'copy-button'}
      aria-label={copied ? 'Copied' : label}
      title={copied ? 'Copied' : label}
      onClick={() => void copy()}
    >
      <Icon name={copied ? 'check' : 'copy'} size={13} className={copied ? 'ok' : undefined} />
    </button>
  );
}

export function Spinner({ size = 14 }: { size?: number }) {
  return <span className="spinner" style={{ width: size, height: size }} role="status" aria-label="Working" />;
}

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: 'default' | 'primary' | 'ghost' | 'danger';
  icon?: IconName;
  busy?: boolean;
  small?: boolean;
}

export function Button({ variant = 'default', icon, busy, small, children, className, disabled, ...rest }: ButtonProps) {
  const classes = ['btn', `btn-${variant}`, small ? 'btn-small' : '', !children ? 'btn-icon' : '', className ?? '']
    .filter(Boolean)
    .join(' ');
  return (
    <button type="button" className={classes} disabled={disabled || busy} {...rest}>
      {busy ? <Spinner size={small ? 11 : 13} /> : icon ? <Icon name={icon} size={small ? 13 : 15} /> : null}
      {children}
    </button>
  );
}

export function Dialog({
  title,
  onClose,
  children,
  footer,
  wide,
}: {
  title: string;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  wide?: boolean;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    const previous = document.activeElement as HTMLElement | null;
    // Focus the first field so the dialog is usable from the keyboard at once.
    ref.current?.querySelector<HTMLElement>('input, select, textarea, button.btn-primary, [data-autofocus]')?.focus();
    return () => {
      window.removeEventListener('keydown', onKey);
      previous?.focus?.();
    };
  }, [onClose]);
  return (
    <div className="overlay" onMouseDown={(event) => event.target === event.currentTarget && onClose()}>
      <div className={wide ? 'dialog dialog-wide' : 'dialog'} role="dialog" aria-modal="true" aria-label={title} ref={ref}>
        <header className="dialog-header">
          <h2>{title}</h2>
          <Button variant="ghost" icon="x" onClick={onClose} aria-label="Close" />
        </header>
        <div className="dialog-body">{children}</div>
        {footer ? <footer className="dialog-footer">{footer}</footer> : null}
      </div>
    </div>
  );
}

/** A button that opens a floating panel anchored to it. */
export function Popover({
  trigger,
  children,
  align = 'start',
  placement = 'top',
}: {
  trigger: (props: { open: boolean; toggle: () => void }) => ReactNode;
  children: (close: () => void) => ReactNode;
  align?: 'start' | 'end';
  placement?: 'top' | 'bottom';
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDown = (event: MouseEvent) => {
      if (ref.current && !ref.current.contains(event.target as Node)) setOpen(false);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.stopPropagation();
        setOpen(false);
      }
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey, true);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey, true);
    };
  }, [open]);
  return (
    <span className="popover-anchor" ref={ref}>
      {trigger({ open, toggle: () => setOpen((value) => !value) })}
      {open ? <div className={`popover popover-${placement} popover-${align}`}>{children(() => setOpen(false))}</div> : null}
    </span>
  );
}

/** Keeps a scroll container pinned to the bottom while the user is there. */
export function useStickToBottom(dependency: unknown) {
  // The container is held in state, not a ref object: it mounts after the first
  // render (the view shows a spinner while loading), and the listener must attach then.
  const [element, setElement] = useState<HTMLDivElement | null>(null);
  const pinned = useRef(true);
  const [atBottom, setAtBottom] = useState(true);
  useEffect(() => {
    if (!element) return;
    const onScroll = () => {
      pinned.current = element.scrollHeight - element.scrollTop - element.clientHeight < 80;
      setAtBottom(pinned.current);
    };
    element.addEventListener('scroll', onScroll, { passive: true });
    return () => element.removeEventListener('scroll', onScroll);
  }, [element]);
  useLayoutEffect(() => {
    if (element && pinned.current) element.scrollTop = element.scrollHeight;
  }, [element, dependency]);
  return {
    ref: setElement,
    atBottom,
    scrollToBottom: () => {
      pinned.current = true;
      setAtBottom(true);
      if (element) element.scrollTop = element.scrollHeight;
    },
  };
}

export function Segmented<T extends string>({
  value,
  options,
  onChange,
  label,
}: {
  value: T;
  options: { value: T; label: string; title?: string }[];
  onChange: (value: T) => void;
  label: string;
}) {
  return (
    <div className="segmented" role="radiogroup" aria-label={label}>
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          role="radio"
          aria-checked={option.value === value}
          className={option.value === value ? 'segment active' : 'segment'}
          title={option.title}
          onClick={() => onChange(option.value)}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}
