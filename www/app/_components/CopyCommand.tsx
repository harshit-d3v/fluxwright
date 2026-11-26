"use client";

import { useState } from "react";

export type CommandPreset = { label: string; command: string };

export function CopyCommand({
  command,
  presets,
}: {
  command?: string;
  presets?: CommandPreset[];
}) {
  const [activeIdx, setActiveIdx] = useState(0);
  const [state, setState] = useState<"idle" | "copied" | "failed">("idle");

  const activeCommand = presets && presets.length > 0 ? presets[activeIdx]?.command ?? "" : command ?? "";

  async function copy() {
    try {
      await navigator.clipboard.writeText(activeCommand);
      setState("copied");
    } catch {
      setState("failed");
    }
    window.setTimeout(() => setState("idle"), 2000);
  }

  return (
    <div className="command-wrap">
      {presets && presets.length > 0 ? (
        <div className="command-presets" role="group" aria-label="Install target">
          {presets.map((p, idx) => (
            <button
              key={p.label}
              type="button"
              className={idx === activeIdx ? "preset-btn active" : "preset-btn"}
              onClick={() => {
                setActiveIdx(idx);
                setState("idle");
              }}
            >
              {p.label}
            </button>
          ))}
        </div>
      ) : null}
      <div className="command">
        <code>
          <span className="prompt" aria-hidden="true">
            ${" "}
          </span>
          {activeCommand}
        </code>
        <button type="button" className="command-copy" onClick={copy}>
          {state === "copied" ? "Copied" : state === "failed" ? "Failed" : "Copy"}
          <span className="visually-hidden"> {activeCommand}</span>
        </button>
        <span className="visually-hidden" role="status">
          {state === "copied"
            ? "Copied to clipboard"
            : state === "failed"
              ? "Could not copy; select the command instead"
              : ""}
        </span>
      </div>
    </div>
  );
}
