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
  colorScheme: "dark light",
};

export default function RootLayout({ children }: Readonly<{ children: React.ReactNode }>) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
