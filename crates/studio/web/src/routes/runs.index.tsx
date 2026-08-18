import { Link, createFileRoute, useNavigate } from "@tanstack/react-router"
import { useQuery } from "@tanstack/react-query"
import { ListTree, X } from "lucide-react"

import { queries, runListQuery } from "@/lib/api"
import type { RunStatus, RunSummary } from "@/lib/api"
import {
  RunStatusPill,
  formatDuration,
  formatRelative,
} from "@/components/status"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Skeleton } from "@/components/ui/skeleton"

const STATUSES: Array<RunStatus> = [
  "running",
  "success",
  "failure",
  "cancelled",
  "errored",
]

type RunFilters = { workflow?: string; status?: RunStatus }

export const Route = createFileRoute("/runs/")({
  // Filters live in the URL so a filtered view can be linked and reloaded.
  validateSearch: (search: Record<string, unknown>): RunFilters => ({
    workflow: typeof search.workflow === "string" ? search.workflow : undefined,
    status: STATUSES.includes(search.status as RunStatus)
      ? (search.status as RunStatus)
      : undefined,
  }),
  component: RunsScreen,
})

function RunsScreen() {
  const filters = Route.useSearch()
  const navigate = useNavigate({ from: Route.fullPath })

  const runs = useQuery({
    ...runListQuery(filters),
    // A run started elsewhere — the CLI, another tab — should show up without
    // a manual refresh, and finished runs need their final status.
    refetchInterval: 4000,
  })
  const workflows = useQuery(queries.workflows())

  const setFilter = (next: Partial<RunFilters>) =>
    navigate({ search: (current) => ({ ...current, ...next }) })

  const filtered = Boolean(filters.workflow || filters.status)

  return (
    <div className="flex flex-col gap-6 p-8">
      <header className="flex flex-col gap-1">
        <h1 className="font-mono text-xl font-bold tracking-tight">Runs</h1>
        <p className="text-muted-foreground text-sm">
          Everything this workspace has run, newest first.
        </p>
      </header>

      <div className="flex flex-wrap items-center gap-2">
        <Select
          value={filters.workflow ?? "all"}
          onValueChange={(value) =>
            setFilter({ workflow: value === "all" ? undefined : value })
          }
        >
          <SelectTrigger size="sm" className="w-56" aria-label="Filter by workflow">
            <SelectValue placeholder="All workflows" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="all">All workflows</SelectItem>
            {workflows.data
              ?.filter((workflow) => !workflow.error)
              .map((workflow) => (
                <SelectItem key={workflow.id} value={workflow.id}>
                  {workflow.name}
                </SelectItem>
              ))}
          </SelectContent>
        </Select>

        <Select
          value={filters.status ?? "all"}
          onValueChange={(value) =>
            setFilter({
              status: value === "all" ? undefined : (value as RunStatus),
            })
          }
        >
          <SelectTrigger size="sm" className="w-40" aria-label="Filter by status">
            <SelectValue placeholder="Any status" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="all">Any status</SelectItem>
            {STATUSES.map((status) => (
              <SelectItem key={status} value={status}>
                {status}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>

        {filtered ? (
          <Button
            variant="ghost"
            size="sm"
            onClick={() => navigate({ search: {} })}
          >
            <X className="size-3.5" aria-hidden />
            Clear
          </Button>
        ) : null}

        <span className="text-muted-foreground ml-auto font-mono text-xs tabular-nums">
          {runs.data
            ? `${runs.data.length} ${runs.data.length === 1 ? "run" : "runs"}`
            : ""}
        </span>
      </div>

      {runs.isPending ? (
        <div className="flex flex-col gap-2">
          {[0, 1, 2].map((index) => (
            <Skeleton key={index} className="h-16 w-full rounded-lg" />
          ))}
        </div>
      ) : null}

      {runs.data?.length === 0 ? (
        <div className="bg-card flex items-start gap-3 rounded-lg border p-5">
          <ListTree
            className="text-muted-foreground mt-0.5 size-4 shrink-0"
            aria-hidden
          />
          <div className="flex flex-col gap-1">
            <p className="text-sm font-medium">
              {filtered ? "No runs match these filters" : "No runs yet"}
            </p>
            <p className="text-muted-foreground text-sm">
              {filtered
                ? "Clear the filters to see the rest."
                : "Open a workflow and press Run to start one."}
            </p>
          </div>
        </div>
      ) : null}

      {runs.data && runs.data.length > 0 ? (
        <ul className="flex flex-col gap-2">
          {runs.data.map((run) => (
            <li key={run.id}>
              <RunRow run={run} />
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  )
}

function RunRow({ run }: { run: RunSummary }) {
  return (
    <Link
      to="/runs/$runId"
      params={{ runId: run.id }}
      className="bg-card hover:border-muted-foreground/40 focus-visible:outline-ring flex items-center gap-4 rounded-lg border p-4 text-sm transition-colors focus-visible:outline-2 focus-visible:outline-offset-2"
    >
      <span className="text-muted-foreground w-12 shrink-0 font-mono text-xs tabular-nums">
        #{run.id}
      </span>

      <div className="flex min-w-0 flex-1 flex-col gap-1">
        <div className="flex items-center gap-2">
          <span className="truncate font-medium">{run.workflow_name}</span>
          <Badge variant="secondary" className="font-mono text-[10px]">
            {run.event}
          </Badge>
        </div>
        <span className="text-muted-foreground truncate font-mono text-xs">
          {run.workflow_path}
        </span>
      </div>

      <span className="text-muted-foreground shrink-0 font-mono text-xs tabular-nums">
        {formatDuration(run.duration_ms)}
      </span>
      <span className="text-muted-foreground w-20 shrink-0 text-right font-mono text-xs">
        {formatRelative(run.started_at)}
      </span>
      <RunStatusPill status={run.status} />
    </Link>
  )
}
