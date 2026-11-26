import type { Metadata, Viewport } from "next";
import { Instrument_Serif, Inter_Tight, JetBrains_Mono } from "next/font/google";
import "./globals.css";

const sans = Inter_Tight({
  subsets: ["latin"],
  weight: ["400", "500", "600", "700"],
  variable: "--font-sans",
  display: "swap",
});

const serif = Instrument_Serif({
  subsets: ["latin"],
  weight: "400",
  style: ["normal", "italic"],
  variable: "--font-serif",
  display: "swap",
});

const mono = JetBrains_Mono({
  subsets: ["latin"],
  weight: ["400", "500", "600"],
  variable: "--font-mono",
  display: "swap",
});

export const metadata: Metadata = {
  metadataBase: new URL("https://fluxwright.vercel.app"),
  title: "Fluxwright — Pooled Chrome Fleet & Hybrid Scraping Engine",
  description:
    "Run hundreds of isolated headless Chrome jobs on one machine with a pure-Rust HTTP fast path, pluggable light-CDP tier, built-in E2E testing, managed sessions + Live View HITL, 22-tool MCP server, and cluster orchestration for Node, Python, and Rust.",
  icons: { icon: "/favicon.svg" },
  openGraph: {
    title: "Fluxwright — Pooled Chrome Fleet & Hybrid Scraping Engine",
    description:
      "Pure-Rust HTTP fast path with automatic pooled Chromium escalation, crash recovery, E2E testing batteries, CDP gateway, live HITL dashboard, and 22-tool MCP server.",
    url: "https://fluxwright.vercel.app",
    siteName: "Fluxwright",
    type: "website",
  },
};

export const viewport: Viewport = {
  colorScheme: "light dark",
  themeColor: [
    { media: "(prefers-color-scheme: light)", color: "#f6f6f5" },
    { media: "(prefers-color-scheme: dark)", color: "#0d0e10" },
  ],
};

const THEME_INIT_SCRIPT = `(function(){try{var t=localStorage.getItem('fw-theme');if(t==='light'||t==='dark'){document.documentElement.dataset.theme=t;}}catch(_){}})();`;

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html
      lang="en"
      className={`${sans.variable} ${serif.variable} ${mono.variable}`}
      suppressHydrationWarning
    >
      <head>
        <script dangerouslySetInnerHTML={{ __html: THEME_INIT_SCRIPT }} />
      </head>
      <body>
        <div className="scroll-progress" aria-hidden="true" />
        <a className="skip-link" href="#main">
          Skip to content
        </a>
        {children}
      </body>
    </html>
  );
}
