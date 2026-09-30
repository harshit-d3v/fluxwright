// Shared by the server-rendered page and the client header: no "use client" here.

export const SECTIONS = [
  { href: "#features", label: "Features" },
  { href: "#how", label: "How it works" },
  { href: "#code", label: "Code" },
  { href: "#benchmarks", label: "Benchmarks" },
  { href: "#install", label: "Install" },
];

export const GITHUB = "https://github.com/harshit-d3v/fluxwright";
export const NPM = "https://www.npmjs.com/package/fluxwright";

export function Logo() {
  return (
    <svg className="logo" viewBox="0 0 32 32" aria-hidden="true" focusable="false">
      <rect width="32" height="32" rx="7" className="logo-bg" />
      <rect x="6" y="7" width="5" height="5" rx="1" className="logo-on" />
      <rect x="13.5" y="7" width="5" height="5" rx="1" className="logo-on" />
      <rect x="21" y="7" width="5" height="5" rx="1" className="logo-off" />
      <rect x="6" y="14" width="5" height="5" rx="1" className="logo-on" />
      <rect x="13.5" y="14" width="5" height="5" rx="1" className="logo-off" />
      <rect x="21" y="14" width="5" height="5" rx="1" className="logo-off" />
      <rect x="6" y="21" width="5" height="5" rx="1" className="logo-on" />
      <rect x="13.5" y="21" width="5" height="5" rx="1" className="logo-on" />
      <rect x="21" y="21" width="5" height="5" rx="1" className="logo-on" />
    </svg>
  );
}
