"use client";

import { useState } from "react";

export function CopyCommand({ command }: { command: string }) {
  const [state, setState] = useState<"idle" | "copied" | "failed">("idle");

  async function copy() {
    try {
      await navigator.clipboard.writeText(command);
      setState("copied");
    } catch {
      setState("failed");
    }
    window.setTimeout(() => setState("idle"), 2000);
  }

  return (
    <div className="command">
      <code>
        <span className="prompt" aria-hidden="true">$ </span>
        {command}
      </code>
      <button type="button" className="command-copy" onClick={copy}>
        {state === "copied" ? "Copied" : state === "failed" ? "Copy failed" : "Copy"}
        <span className="visually-hidden"> {command}</span>
      </button>
      <span className="visually-hidden" role="status">
        {state === "copied" ? "Copied to clipboard" : state === "failed" ? "Could not copy; select the command instead" : ""}
      </span>
    </div>
  );
}
