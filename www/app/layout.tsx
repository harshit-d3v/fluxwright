import type { Metadata, Viewport } from "next";
import { Barlow, Barlow_Condensed, JetBrains_Mono } from "next/font/google";
import "./globals.css";

const text = Barlow({
  subsets: ["latin"],
  weight: ["400", "500", "600"],
  variable: "--font-barlow",
  display: "swap",
});

const display = Barlow_Condensed({
  subsets: ["latin"],
  weight: ["500", "600"],
  variable: "--font-barlow-condensed",
  display: "swap",
});

const code = JetBrains_Mono({
  subsets: ["latin"],
  weight: ["400", "500"],
  variable: "--font-jetbrains",
  display: "swap",
});

export const metadata: Metadata = {
  metadataBase: new URL("https://fluxwright.vercel.app"),
  title: "Fluxwright: run a fleet of headless Chrome",
  description:
    "A Rust engine that pools Chrome with isolated pages, built-in E2E testing (expect, .zip tracing, video, test runners), a pooled CDP gateway, live HITL dashboard, AI snapshots and Markdown extraction, 22-tool MCP server, and HTTP server/cluster mode for Node, Python, and Rust.",
  icons: { icon: "/favicon.svg" },
  openGraph: {
    title: "Fluxwright: run a fleet of headless Chrome",
    description:
      "Pooled headless Chrome fleet with Playwright-style APIs, E2E testing batteries, CDP WebSocket gateway, live HITL dashboard, AI snapshots, and HTTP/cluster orchestration.",
    url: "https://fluxwright.vercel.app",
    siteName: "Fluxwright",
    type: "website",
  },
};

export const viewport: Viewport = {
  colorScheme: "light dark",
  themeColor: [
    { media: "(prefers-color-scheme: light)", color: "#f2f4f6" },
    { media: "(prefers-color-scheme: dark)", color: "#0c1117" },
  ],
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" className={`${text.variable} ${display.variable} ${code.variable}`}>
      <body>
        <a className="skip-link" href="#main">
          Skip to content
        </a>
        {children}
      </body>
    </html>
  );
}
