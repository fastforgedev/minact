/**
 * The frame every screen sits in: a fixed nav rail plus a workspace footer.
 *
 * Runs, Artifacts and Inspector are listed but disabled — M1 is discovery
 * only, and hiding the destinations would misrepresent what Studio is.
 */
import { Link, useRouterState } from "@tanstack/react-router"
import { useQuery } from "@tanstack/react-query"
import { FileCode2, FlaskConical, ListTree, Package } from "lucide-react"
import type { LucideIcon } from "lucide-react"

import { queries } from "@/lib/api"
import { cn } from "@/lib/utils"
import { ThemeToggle } from "@/components/theme-toggle"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"

type NavItem = {
  label: string
  to?: string
  icon: LucideIcon
  soon?: string
}

const NAV: Array<NavItem> = [
  { label: "Workflows", to: "/workflows", icon: FileCode2 },
  { label: "Runs", to: "/runs", icon: ListTree },
  { label: "Artifacts", to: "/artifacts", icon: Package },
  {
    label: "Inspector",
    icon: FlaskConical,
    soon: "Arrives with context snapshots",
  },
]

export function AppShell({ children }: { children: React.ReactNode }) {
  const meta = useQuery(queries.meta())
  const pathname = useRouterState({ select: (s) => s.location.pathname })

  return (
    <div className="bg-background text-foreground flex h-svh overflow-hidden">
      <nav className="bg-card flex w-56 shrink-0 flex-col border-r">
        <div className="flex h-14 items-center gap-2 border-b px-4">
          <span className="font-mono text-sm font-bold tracking-tight">
            minact
          </span>
          <span className="text-primary font-mono text-sm">studio</span>
        </div>

        <ul className="flex flex-col gap-0.5 p-2">
          {NAV.map((item) => (
            <li key={item.label}>
              <NavRow item={item} pathname={pathname} />
            </li>
          ))}
        </ul>

        <div className="text-muted-foreground mt-auto space-y-1 border-t p-4 font-mono text-[11px]">
          <p className="text-foreground/70 truncate" title={meta.data?.workspace}>
            {meta.data?.workspace ?? "…"}
          </p>
          <p>
            {meta.data
              ? `${meta.data.runner.os} · ${meta.data.runner.arch} · v${meta.data.version}`
              : "connecting…"}
          </p>
        </div>
      </nav>

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-14 shrink-0 items-center justify-end gap-2 border-b px-5">
          <ThemeToggle />
        </header>
        {/* The run screen manages its own panes, so give it a definite
            height to fill and let it own the scrolling inside. */}
        <main className="min-h-0 min-w-0 flex-1 overflow-y-auto">
          {children}
        </main>
      </div>
    </div>
  )
}

function NavRow({ item, pathname }: { item: NavItem; pathname: string }) {
  const className =
    "flex items-center gap-2.5 rounded-md px-2.5 py-1.5 text-sm transition-colors"

  if (!item.to) {
    return (
      <Tooltip>
        <TooltipTrigger asChild>
          <span
            aria-disabled="true"
            className={cn(className, "text-muted-foreground/50 cursor-default")}
          >
            <item.icon className="size-4" aria-hidden />
            {item.label}
          </span>
        </TooltipTrigger>
        <TooltipContent side="right">{item.soon}</TooltipContent>
      </Tooltip>
    )
  }

  const active = pathname.startsWith(item.to)

  return (
    <Link
      to={item.to}
      className={cn(
        className,
        active
          ? "bg-accent text-accent-foreground font-medium"
          : "text-muted-foreground hover:bg-accent/50 hover:text-foreground",
      )}
    >
      <item.icon className="size-4" aria-hidden />
      {item.label}
    </Link>
  )
}
