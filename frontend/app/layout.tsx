import type { Metadata } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "khive — research knowledge graph",
  description: "Build domain-specific knowledge graphs that grow with your work.",
};

export default function RootLayout({
  children,
}: Readonly<{ children: React.ReactNode }>) {
  return (
    <html lang="en">
      <body className="antialiased">{children}</body>
    </html>
  );
}
