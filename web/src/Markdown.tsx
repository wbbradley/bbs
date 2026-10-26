import { memo } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";

// Agent text is untrusted. react-markdown builds React elements from the
// Markdown tree and never renders raw HTML (no rehype-raw), so a message can
// format but not script. Links open in a new tab without a referrer.
const components: Components = {
  a: ({ href, children }) => (
    <a href={href} target="_blank" rel="noreferrer noopener">
      {children}
    </a>
  ),
  img: ({ alt }) => <span className="faint">[image{alt ? `: ${alt}` : ""}]</span>,
};

const safe = (url: string) => (/^(https?:|mailto:|#|\/)/i.test(url) ? url : "");

export const Markdown = memo(function Markdown({ text }: { text: string }) {
  return (
    <div className="md">
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components} urlTransform={safe}>
        {text}
      </ReactMarkdown>
    </div>
  );
});
