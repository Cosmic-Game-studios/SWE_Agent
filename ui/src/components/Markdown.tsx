import { type ReactNode, isValidElement, memo, useRef } from 'react';
import ReactMarkdown from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import remarkGfm from 'remark-gfm';

import { CopyButton } from './ui';

const remarkPlugins = [remarkGfm];
const rehypePlugins = [rehypeHighlight];

function CodeBlock({ children }: { children?: ReactNode }) {
  const ref = useRef<HTMLPreElement>(null);
  const className = isValidElement<{ className?: string }>(children) ? children.props.className : undefined;
  const language = /language-([\w+#-]+)/.exec(className ?? '')?.[1];
  return (
    <div className="code-block">
      <div className="code-block-bar">
        <span>{language ?? 'text'}</span>
        <CopyButton text={() => ref.current?.textContent ?? ''} label="Copy code" />
      </div>
      <pre ref={ref}>{children}</pre>
    </div>
  );
}

/** Assistant text. Memoised: finished messages never re-render while new text streams in. */
export const Markdown = memo(function Markdown({ text }: { text: string }) {
  return (
    <div className="markdown">
      <ReactMarkdown
        remarkPlugins={remarkPlugins}
        rehypePlugins={rehypePlugins}
        components={{
          pre: ({ children }) => <CodeBlock>{children}</CodeBlock>,
          a: ({ children, href }) => (
            <a href={href} target="_blank" rel="noreferrer noopener">
              {children}
            </a>
          ),
        }}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
});
