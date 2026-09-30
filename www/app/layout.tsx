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
    "A Rust engine that pools Chrome, gives every job a fresh isolated page, queues work when the fleet is full, recycles bloated browsers and retries crashes. For Node, Rust and AI agents over MCP.",
  icons: { icon: "/favicon.svg" },
  openGraph: {
    title: "Fluxwright: run a fleet of headless Chrome",
    description:
      "Pooling, admission control, recycling and crash recovery for headless Chrome, with a Playwright-style API.",
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
