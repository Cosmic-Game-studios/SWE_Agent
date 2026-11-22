import { memo, useMemo, useState } from 'react';

import { Icon } from './ui';

export interface DiffLine {
  type: 'add' | 'del' | 'ctx' | 'hunk' | 'meta';
  text: string;
  oldNo?: number;
  newNo?: number;
}

export interface DiffFile {
  path: string;
  added: number;
  removed: number;
  created: boolean;
  deleted: boolean;
  lines: DiffLine[];
}

/** Parse unified diff text (one or many files) into renderable lines. */
export function parseDiff(text: string): DiffFile[] {
  const files: DiffFile[] = [];
  let file: DiffFile | null = null;
  let oldNo = 0;
  let newNo = 0;
  const start = (path: string): DiffFile => {
    const next: DiffFile = { path, added: 0, removed: 0, created: false, deleted: false, lines: [] };
    files.push(next);
    return next;
  };
  for (const line of text.split('\n')) {
    if (line.startsWith('diff --git ')) {
      const match = /^diff --git a\/(.*) b\/(.*)$/.exec(line);
      file = start(match?.[2] ?? line.slice(11));
      continue;
    }
    if (line.startsWith('--- ')) {
      // A bare `---/+++` pair (no `diff --git` line) also starts a file.
      if (!file || file.lines.length) file = start(line.slice(4).replace(/^a\//, ''));
      if (line.slice(4).trim() === '/dev/null') file.created = true;
      continue;
    }
    if (!file) continue;
    if (line.startsWith('+++ ')) {
      const target = line.slice(4).trim();
      if (target === '/dev/null') file.deleted = true;
      else file.path = target.replace(/^b\//, '');
      continue;
    }
    if (line.startsWith('new file mode')) {
      file.created = true;
      continue;
    }
    if (line.startsWith('deleted file mode')) {
      file.deleted = true;
      continue;
    }
    if (line.startsWith('@@')) {
      const match = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
      oldNo = match ? Number(match[1]) : 0;
      newNo = match ? Number(match[2]) : 0;
      file.lines.push({ type: 'hunk', text: line });
      continue;
    }
    if (line.startsWith('+')) {
      file.lines.push({ type: 'add', text: line.slice(1), newNo: newNo++ });
      file.added += 1;
    } else if (line.startsWith('-')) {
      file.lines.push({ type: 'del', text: line.slice(1), oldNo: oldNo++ });
      file.removed += 1;
    } else if (line.startsWith(' ')) {
      file.lines.push({ type: 'ctx', text: line.slice(1), oldNo: oldNo++, newNo: newNo++ });
    } else if (line.startsWith('Binary') || line.startsWith('\\')) {
      file.lines.push({ type: 'meta', text: line });
    }
  }
  return files;
}

const MAX_LINES = 400;

export const DiffLines = memo(function DiffLines({ lines }: { lines: DiffLine[] }) {
  const [showAll, setShowAll] = useState(false);
  const visible = showAll ? lines : lines.slice(0, MAX_LINES);
  if (!lines.length) return <div className="diff-empty">No textual changes.</div>;
  return (
    <div className="diff">
      <table>
        <tbody>
          {visible.map((line, index) =>
            line.type === 'hunk' || line.type === 'meta' ? (
              <tr key={index} className={`diff-${line.type}`}>
                <td colSpan={3}>{line.text}</td>
              </tr>
            ) : (
              <tr key={index} className={`diff-${line.type}`}>
                <td className="diff-no">{line.oldNo ?? ''}</td>
                <td className="diff-no">{line.newNo ?? ''}</td>
                <td className="diff-code">
                  <span className="diff-sign">{line.type === 'add' ? '+' : line.type === 'del' ? '-' : ' '}</span>
                  {line.text}
                </td>
              </tr>
            ),
          )}
        </tbody>
      </table>
      {lines.length > MAX_LINES && !showAll ? (
        <button type="button" className="diff-more" onClick={() => setShowAll(true)}>
          Show {lines.length - MAX_LINES} more lines
        </button>
      ) : null}
    </div>
  );
});

export function DiffStat({ added, removed }: { added: number; removed: number }) {
  return (
    <span className="diff-stat">
      {added > 0 ? <span className="plus">+{added}</span> : null}
      {removed > 0 ? <span className="minus">−{removed}</span> : null}
    </span>
  );
}

/** One file of a diff with a collapsible header. */
export function DiffFileView({ file, defaultOpen = true }: { file: DiffFile; defaultOpen?: boolean }) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <section className="diff-file">
      <button type="button" className="diff-file-header" onClick={() => setOpen(!open)} aria-expanded={open}>
        <Icon name={open ? 'chevron-down' : 'chevron-right'} size={14} />
        <span className="diff-path" title={file.path}>
          {file.path}
        </span>
        {file.created ? <span className="tag tag-green">new</span> : null}
        {file.deleted ? <span className="tag tag-red">deleted</span> : null}
        <DiffStat added={file.added} removed={file.removed} />
      </button>
      {open ? <DiffLines lines={file.lines} /> : null}
    </section>
  );
}

/** Render a single-file unified diff (as attached to an edit tool call). */
export function InlineDiff({ diff }: { diff: string }) {
  const files = useMemo(() => parseDiff(diff), [diff]);
  const lines = files.flatMap((file) => file.lines);
  return <DiffLines lines={lines} />;
}
