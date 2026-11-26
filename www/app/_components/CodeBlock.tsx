"use client";

import { useState } from "react";

export function CodeBlock({ children, label }: { children: string; label: string }) {
  const [copied, setCopied] = useState(false);

  async function onCopy() {
    try {
      await navigator.clipboard.writeText(children);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1800);
    } catch {
      setCopied(false);
    }
  }

  return (
    <div className="code-frame">
      <div className="code-toolbar">
        <span className="code-label">{label}</span>
        <button type="button" className="code-copy" onClick={onCopy}>
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      <pre className="code" aria-label={label}>
        <code>{children}</code>
      </pre>
    </div>
  );
}
