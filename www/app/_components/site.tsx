// Shared by server and client components: keep pure data and static JSX only.

export const SECTIONS = [
  { href: "#features", label: "Capabilities" },
  { href: "#how", label: "Architecture" },
  { href: "#code", label: "Code" },
  { href: "#benchmarks", label: "Benchmarks" },
  { href: "#install", label: "Install" },
  { href: "#scope", label: "Scope" },
];

export const GITHUB = "https://github.com/harshit-d3v/fluxwright";
export const NPM = "https://www.npmjs.com/package/fluxwright";
export const PYPI = "https://pypi.org/project/fluxwright/";
export const RELEASES = "https://github.com/harshit-d3v/fluxwright/releases";
export const GHCR = "https://github.com/harshit-d3v/fluxwright/pkgs/container/fluxwright";
export const SERVER_DOCS = "https://github.com/harshit-d3v/fluxwright/blob/main/docs/SERVER.md";
export const CLUSTER_DOCS = "https://github.com/harshit-d3v/fluxwright/blob/main/docs/CLUSTER.md";

export function Logo() {
  return (
    <svg className="logo" viewBox="0 0 32 32" aria-hidden="true" focusable="false">
      <rect width="32" height="32" rx="7" className="logo-bg" />
      <rect x="6" y="7" width="5" height="5" rx="1.2" className="logo-on" />
      <rect x="13.5" y="7" width="5" height="5" rx="1.2" className="logo-on" />
      <rect x="21" y="7" width="5" height="5" rx="1.2" className="logo-off" />
      <rect x="6" y="14" width="5" height="5" rx="1.2" className="logo-on" />
      <rect x="13.5" y="14" width="5" height="5" rx="1.2" className="logo-off" />
      <rect x="21" y="14" width="5" height="5" rx="1.2" className="logo-off" />
      <rect x="6" y="21" width="5" height="5" rx="1.2" className="logo-on" />
      <rect x="13.5" y="21" width="5" height="5" rx="1.2" className="logo-on" />
      <rect x="21" y="21" width="5" height="5" rx="1.2" className="logo-on" />
    </svg>
  );
}
