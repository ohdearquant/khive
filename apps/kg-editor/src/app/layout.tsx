import type { Metadata, Viewport } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "KG Studio · Semantic review for khive",
  description:
    "Inspect attributed knowledge-graph changes, evidence, checks, and affected context before they land.",
  applicationName: "khive KG Studio",
  icons: {
    icon: "/favicon.ico",
  },
};

export const viewport: Viewport = {
  colorScheme: "light",
  themeColor: "#f5f3ee",
};

export default function RootLayout({ children }: Readonly<{ children: React.ReactNode }>) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
