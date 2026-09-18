import React from "react";

export interface TabItem<T extends string> {
  id: T;
  label: string;
}

interface TabsProps<T extends string> {
  tabs: TabItem<T>[];
  active: T;
  onChange: (id: T) => void;
  /** Names the group for a screen reader, since the buttons alone do not say what they switch. */
  ariaLabel: string;
  className?: string;
}

/**
 * Segmented control for switching between views of the same content.
 *
 * A pressed-button group rather than `role="tablist"`. The tab role promises
 * arrow-key navigation, a roving tabindex and an `aria-controls` pointing at a
 * `role="tabpanel"`, and declaring the role without delivering those is worse
 * than not declaring it: a screen reader announces "tab 1 of 2" for a control
 * that answers no arrow key. `aria-pressed` is a complete contract that plain
 * buttons already satisfy.
 *
 * Generic over the id union so a caller keeps its own literal type across the
 * boundary instead of widening to `string` and losing exhaustiveness where it
 * branches on the result.
 */
export function Tabs<T extends string>({
  tabs,
  active,
  onChange,
  ariaLabel,
  className = "",
}: TabsProps<T>): React.ReactElement {
  return (
    <div
      role="group"
      aria-label={ariaLabel}
      className={`inline-flex items-center gap-0.5 p-0.5 rounded-lg bg-mid-gray/10 ${className}`}
    >
      {tabs.map((tab) => {
        const selected = tab.id === active;
        return (
          <button
            key={tab.id}
            type="button"
            aria-pressed={selected}
            onClick={() => onChange(tab.id)}
            className={`px-2.5 py-1 text-xs font-medium rounded-md transition-colors cursor-pointer ${
              selected
                ? "bg-background text-text"
                : "text-text/50 hover:text-text/80"
            }`}
          >
            {tab.label}
          </button>
        );
      })}
    </div>
  );
}
