"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";

const NAV = [
  { href: "/", label: "Dashboard", icon: "◈" },
  { href: "/kg", label: "KG Explorer", icon: "◎" },
  { href: "/tasks", label: "GTD Board", icon: "▤" },
  { href: "/swarm", label: "Swarm", icon: "◉" },
];

export function Sidebar() {
  const pathname = usePathname();
  return (
    <aside className="flex h-screen w-48 flex-col border-r border-neutral-800 bg-neutral-950 font-mono text-sm">
      <div className="border-b border-neutral-800 px-4 py-3">
        <span className="font-bold tracking-tight text-white">khive</span>
        <span className="ml-1 text-xs text-neutral-500">v0.1</span>
      </div>
      <nav className="flex-1 px-2 py-2">
        {NAV.map(({ href, label, icon }) => {
          const active =
            href === "/" ? pathname === "/" : pathname.startsWith(href);
          return (
            <Link
              key={href}
              href={href}
              className={[
                "flex items-center gap-2 rounded px-3 py-2 transition-colors",
                active
                  ? "bg-neutral-800 text-white"
                  : "text-neutral-400 hover:bg-neutral-900 hover:text-neutral-200",
              ].join(" ")}
            >
              <span className="text-base leading-none">{icon}</span>
              <span>{label}</span>
            </Link>
          );
        })}
      </nav>
      <div className="border-t border-neutral-800 px-4 py-2 text-xs text-neutral-600">
        research kg runtime
      </div>
    </aside>
  );
}
