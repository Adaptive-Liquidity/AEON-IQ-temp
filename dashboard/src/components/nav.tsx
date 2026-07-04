"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { Brain, LayoutDashboard, Search, LogOut, ShieldCheck, Sparkles, Waypoints } from "lucide-react";
import { useSession, signOut } from "next-auth/react";

const links = [
  { href: "/",                label: "Overview",        icon: LayoutDashboard },
  { href: "/memory-explorer", label: "Memory Explorer", icon: Search },
  { href: "/cognition",       label: "Cognition",       icon: Sparkles },
  { href: "/lifecycle",       label: "Lifecycle",       icon: Waypoints },
];

export default function Nav() {
  const pathname       = usePathname();
  const { data: session } = useSession();

  if (pathname.startsWith("/login")) return null;

  return (
    <nav className="border-b border-zinc-800 bg-zinc-900">
      <div className="max-w-6xl mx-auto px-4 h-14 flex items-center gap-8">
        <Link href="/" className="flex items-center gap-2 font-bold text-green-400 shrink-0">
          <Brain className="w-5 h-5" />
          MemoryOS
        </Link>

        <div className="flex items-center gap-1">
          {links.map(({ href, label, icon: Icon }) => (
            <Link
              key={href}
              href={href}
              className={`flex items-center gap-1.5 px-3 py-1.5 rounded-md text-sm transition-colors ${
                pathname === href
                  ? "bg-zinc-800 text-zinc-100"
                  : "text-zinc-400 hover:text-zinc-100 hover:bg-zinc-800"
              }`}
            >
              <Icon className="w-4 h-4" />
              {label}
            </Link>
          ))}
        </div>

        <div className="ml-auto flex items-center gap-3">
          {session?.user && (
            <>
              <div className="flex items-center gap-1.5 text-xs text-zinc-400">
                {session.user.isAdmin && (
                  <span title="Admin">
                    <ShieldCheck className="w-3.5 h-3.5 text-green-400" />
                  </span>
                )}
                <span className="hidden sm:block max-w-[160px] truncate">
                  {session.user.email}
                </span>
                <span className="font-mono text-zinc-600 text-[10px] hidden md:block">
                  {session.user.agentId}
                </span>
              </div>
              <button
                onClick={() => signOut({ callbackUrl: "/login" })}
                className="flex items-center gap-1 px-2.5 py-1.5 rounded-md text-xs text-zinc-500 hover:text-zinc-100 hover:bg-zinc-800 transition-colors"
                title="Sign out"
              >
                <LogOut className="w-3.5 h-3.5" />
                <span className="hidden sm:block">Sign out</span>
              </button>
            </>
          )}
        </div>
      </div>
    </nav>
  );
}
