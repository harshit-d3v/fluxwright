"use client";

import { useId, useRef, useState, type KeyboardEvent, type ReactNode } from "react";

export type Tab = { title: string; content: ReactNode };

// WAI-ARIA tabs: arrow keys move between tabs (automatic activation), Home and End jump
// to the ends, and only the selected tab sits in the tab order.
export function Tabs({ label, tabs }: { label: string; tabs: Tab[] }) {
  const [selected, setSelected] = useState(0);
  const refs = useRef<(HTMLButtonElement | null)[]>([]);
  const id = useId();

  function onKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    const next =
      e.key === "ArrowRight" ? (selected + 1) % tabs.length
      : e.key === "ArrowLeft" ? (selected - 1 + tabs.length) % tabs.length
      : e.key === "Home" ? 0
      : e.key === "End" ? tabs.length - 1
      : null;
    if (next === null) return;
    e.preventDefault();
    setSelected(next);
    refs.current[next]?.focus();
  }

  return (
    <div className="tabs">
      <div role="tablist" aria-label={label} className="tablist" onKeyDown={onKeyDown}>
        {tabs.map((tab, i) => (
          <button
            key={tab.title}
            ref={(el) => {
              refs.current[i] = el;
            }}
            type="button"
            role="tab"
            id={`${id}-tab-${i}`}
            aria-controls={`${id}-panel-${i}`}
            aria-selected={i === selected}
            tabIndex={i === selected ? 0 : -1}
            className="tab"
            onClick={() => setSelected(i)}
          >
            {tab.title}
          </button>
        ))}
      </div>
      {tabs.map((tab, i) => (
        <div
          key={tab.title}
          role="tabpanel"
          id={`${id}-panel-${i}`}
          aria-labelledby={`${id}-tab-${i}`}
          hidden={i !== selected}
          tabIndex={0}
          className="tabpanel"
        >
          {tab.content}
        </div>
      ))}
    </div>
  );
}
