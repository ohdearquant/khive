import type { Metadata, Viewport } from "next";
import "./globals.css";
import "./showcase.css";

export const metadata: Metadata = {
  title: "khive · Repository atlas",
  description:
    "Explore repository structure, history, hotspots, coupling, and ownership from a reproducible khive graph bundle.",
  applicationName: "khive",
  icons: {
    icon: "/favicon.ico",
  },
};

export const viewport: Viewport = {
  colorScheme: "light dark",
  themeColor: [
    { media: "(prefers-color-scheme: light)", color: "#f5f3ee" },
    { media: "(prefers-color-scheme: dark)", color: "#17140f" },
  ],
};

export default function RootLayout({ children }: Readonly<{ children: React.ReactNode }>) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
