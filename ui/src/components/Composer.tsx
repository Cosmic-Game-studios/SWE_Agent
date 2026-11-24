import { type ClipboardEvent, type DragEvent, type KeyboardEvent, useEffect, useLayoutEffect, useRef, useState } from 'react';

import { type Effort, type ImageRef, type PermissionMode, type SessionMeta, api } from '../api';
import { EFFORTS, MODES, formatTokens, usageSummary } from '../format';
import type { SessionStore } from '../session';
import { attempt, toast, updateSession } from '../store';
import { ModelPicker } from './ModelPicker';
import { Button, Icon, Popover } from './ui';

const MAX_IMAGE_BYTES = 5 * 1024 * 1024;

function readImage(file: File): Promise<ImageRef> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error(`Cannot read ${file.name}`));
    reader.onload = () => {
      const result = String(reader.result);
      resolve({ media_type: file.type, data: result.slice(result.indexOf(',') + 1) });
    };
    reader.readAsDataURL(file);
  });
}

function OptionMenu<T extends string>({
  icon,
  value,
  options,
  onChange,
  title,
}: {
  icon: 'shield' | 'zap';
  value: T;
  options: { value: T; label: string; title: string }[];
  onChange: (value: T) => void;
  title: string;
}) {
  const current = options.find((option) => option.value === value);
  return (
    <Popover
      trigger={({ toggle, open }) => (
        <button type="button" className={`chip chip-${value}`} onClick={toggle} aria-expanded={open} title={title}>
          <Icon name={icon} size={13} />
          <span className="chip-main">{current?.label ?? value}</span>
          <Icon name="chevron-down" size={12} />
        </button>
      )}
    >
      {(close) => (
        <div className="menu" role="menu" aria-label={title}>
          {options.map((option) => (
            <button
              key={option.value}
              type="button"
              role="menuitemradio"
              aria-checked={option.value === value}
              className={option.value === value ? 'menu-item active' : 'menu-item'}
              onClick={() => {
                onChange(option.value);
                close();
              }}
            >
              <span className="menu-title">{option.label}</span>
              <span className="menu-detail">{option.title}</span>
            </button>
          ))}
        </div>
      )}
    </Popover>
  );
}

export function Composer({
  meta,
  store,
  running,
  onOpenSettings,
  onSent,
}: {
  meta: SessionMeta;
  store: SessionStore;
  running: boolean;
  onOpenSettings: () => void;
  onSent: () => void;
}) {
  const draftKey = `swe-draft-${meta.id}`;
  const [text, setText] = useState(() => sessionStorage.getItem(draftKey) ?? '');
  const [images, setImages] = useState<ImageRef[]>([]);
  const [sending, setSending] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [dragging, setDragging] = useState(false);
  const textarea = useRef<HTMLTextAreaElement>(null);
  const fileInput = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const onKey = (event: globalThis.KeyboardEvent) => {
      if (event.key !== '/' || event.ctrlKey || event.metaKey || event.altKey) return;
      const target = event.target as HTMLElement | null;
      if (target?.closest('input, textarea, select, [contenteditable], [role="dialog"]')) return;
      event.preventDefault();
      textarea.current?.focus();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);
  const native = meta.engine === 'native';

  useEffect(() => {
    sessionStorage.setItem(draftKey, text);
  }, [draftKey, text]);

  useEffect(() => {
    if (!running) setStopping(false);
  }, [running]);

  // Grow with the content, up to a share of the window.
  useLayoutEffect(() => {
    const element = textarea.current;
    if (!element) return;
    element.style.height = 'auto';
    element.style.height = `${Math.min(element.scrollHeight, window.innerHeight * 0.4)}px`;
  }, [text]);

  useEffect(() => {
    textarea.current?.focus();
  }, [meta.id]);

  const addFiles = async (files: Iterable<File>) => {
    for (const file of files) {
      if (!file.type.startsWith('image/')) continue;
      if (file.size > MAX_IMAGE_BYTES) {
        toast(`${file.name} is larger than 5 MB.`);
        continue;
      }
      const image = await attempt(() => readImage(file));
      if (image) setImages((current) => [...current, image]);
    }
  };

  const send = async () => {
    const message = text.trim();
    if ((!message && !images.length) || sending) return;
    setSending(true);
    // Clear right away: the message shows up in the timeline (or the queue) at once.
    const previous = { text, images };
    setText('');
    setImages([]);
    try {
      await store.send(message, previous.images);
      onSent();
    } catch (error) {
      setText(previous.text);
      setImages(previous.images);
      toast(error instanceof Error ? error.message : String(error));
    } finally {
      setSending(false);
    }
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) {
      event.preventDefault();
      void send();
    }
  };

  const onPaste = (event: ClipboardEvent<HTMLTextAreaElement>) => {
    const files = Array.from(event.clipboardData.files);
    if (files.some((file) => file.type.startsWith('image/'))) {
      event.preventDefault();
      void addFiles(files);
    }
  };

  const onDrop = (event: DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    setDragging(false);
    void addFiles(Array.from(event.dataTransfer.files));
  };

  const stop = async () => {
    setStopping(true);
    await attempt(() => api.cancel(meta.id));
  };

  const contextShare = meta.context_window ? Math.min(1, meta.context_tokens / meta.context_window) : 0;

  return (
    <div
      className={dragging ? 'composer dragging' : 'composer'}
      onDragOver={(event) => {
        event.preventDefault();
        setDragging(true);
      }}
      onDragLeave={() => setDragging(false)}
      onDrop={onDrop}
    >
      {store.queued.length ? (
        <div className="queued">
          {store.queued.map((message, index) => (
            <div key={index} className="queued-item" title="The agent will see this at its next step">
              <Icon name="clock" size={12} />
              <span>{message}</span>
            </div>
          ))}
        </div>
      ) : null}
      {images.length ? (
        <div className="attachments">
          {images.map((image, index) => (
            <span key={index} className="attachment">
              <img src={`data:${image.media_type};base64,${image.data}`} alt="Attachment" />
              <button
                type="button"
                aria-label="Remove image"
                onClick={() => setImages((current) => current.filter((_, i) => i !== index))}
              >
                <Icon name="x" size={11} />
              </button>
            </span>
          ))}
        </div>
      ) : null}
      <textarea
        ref={textarea}
        value={text}
        rows={1}
        onChange={(event) => setText(event.target.value)}
        onKeyDown={onKeyDown}
        onPaste={onPaste}
        placeholder={running ? 'Add instructions for the agent while it works…' : 'Describe what to build, fix or change…'}
        aria-label="Message"
      />
      <div className="composer-bar">
        {native ? (
          <ModelPicker
            provider={meta.provider}
            model={meta.model}
            onOpenSettings={onOpenSettings}
            onChange={(provider, model) => {
              if (provider && model) void attempt(() => updateSession(meta.id, { provider, model }));
            }}
          />
        ) : (
          <span className="chip chip-static" title="This session runs on a vendor CLI">
            <Icon name="terminal" size={13} />
            <span className="chip-main">{meta.engine === 'claude-cli' ? 'claude' : 'codex'}</span>
            {meta.model ? <span className="chip-sub">{meta.model}</span> : null}
          </span>
        )}
        <OptionMenu<PermissionMode>
          icon="shield"
          title="Permission mode"
          value={meta.mode}
          options={MODES}
          onChange={(mode) => void attempt(() => updateSession(meta.id, { mode }))}
        />
        <OptionMenu<Effort>
          icon="zap"
          title="Reasoning effort"
          value={meta.effort}
          options={EFFORTS}
          onChange={(effort) => void attempt(() => updateSession(meta.id, { effort }))}
        />
        <span className="composer-spacer" />
        {meta.context_tokens > 0 ? (
          <span
            className="context-meter"
            title={`Context in use${meta.context_window ? ` of ${formatTokens(meta.context_window)}` : ''}. Session total: ${usageSummary(meta.usage)}`}
          >
            {meta.context_window ? (
              <span className="meter">
                <span style={{ width: `${Math.round(contextShare * 100)}%` }} />
              </span>
            ) : null}
            {formatTokens(meta.context_tokens)}
          </span>
        ) : null}
        <input
          ref={fileInput}
          type="file"
          accept="image/*"
          multiple
          hidden
          onChange={(event) => {
            void addFiles(Array.from(event.target.files ?? []));
            event.target.value = '';
          }}
        />
        {native ? (
          <Button variant="ghost" icon="image" title="Attach images" aria-label="Attach images" onClick={() => fileInput.current?.click()} />
        ) : null}
        {running ? (
          <Button variant="danger" icon="x" busy={stopping} onClick={stop} title="Stop the agent">
            Stop
          </Button>
        ) : null}
        <Button
          variant="primary"
          icon="arrow-up"
          aria-label={running ? 'Queue message' : 'Send'}
          title={running ? 'Send to the running agent' : 'Send (Enter)'}
          disabled={!text.trim() && !images.length}
          busy={sending}
          onClick={send}
        />
      </div>
    </div>
  );
}
