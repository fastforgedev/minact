import { Link, createFileRoute } from "@tanstack/react-router"
import { useQuery } from "@tanstack/react-query"
import { AlertTriangle, FileWarning } from "lucide-react"

import { queries } from "@/lib/api"
import type { WorkflowSummary } from "@/lib/api"
import { Badge } from "@/components/ui/badge"
import { Skeleton } from "@/components/ui/skeleton"

export const Route = createFileRoute("/workflows/")({
  component: WorkflowsScreen,
})

function WorkflowsScreen() {
  const workflows = useQuery(queries.workflows())

  return (
    <div className="flex flex-col gap-6 p-8">
      <header className="flex flex-col gap-1">
        <h1 className="font-mono text-xl font-bold tracking-tight">
          Workflows
        </h1>
        <p className="text-muted-foreground text-sm">
          Everything minact discovers under this workspace, in the order it
          searches.
        </p>
      </header>

      {workflows.isPending ? <ListSkeleton /> : null}

      {workflows.isError ? (
        <Panel
          icon={AlertTriangle}
          title="Could not reach the Studio API"
          detail={workflows.error.message}
        />
      ) : null}

      {workflows.data?.length === 0 ? (
        <Panel
          icon={FileWarning}
          title="No workflows found"
          detail="minact looks in .minact/workflows/ and .github/workflows/."
        />
      ) : null}

      {workflows.data && workflows.data.length > 0 ? (
        <ul className="flex flex-col gap-2">
          {workflows.data.map((workflow) => (
            <li key={workflow.id}>
              <WorkflowRow workflow={workflow} />
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  )
}

function WorkflowRow({ workflow }: { workflow: WorkflowSummary }) {
  const body = (
    <>
      <div className="flex min-w-0 flex-col gap-1">
        <div className="flex items-center gap-2">
          <span className="truncate font-medium">{workflow.name}</span>
          <Badge variant="outline" className="font-mono text-[10px]">
            {workflow.source}
          </Badge>
          {workflow.error ? (
            <Badge variant="destructive" className="text-[10px]">
              parse error
            </Badge>
          ) : null}
        </div>
        <span className="text-muted-foreground truncate font-mono text-xs">
          {workflow.path}
        </span>
        {workflow.error ? (
          <span className="text-status-failure mt-1 font-mono text-xs">
            {workflow.error}
          </span>
        ) : null}
      </div>

      {!workflow.error ? (
        <div className="flex shrink-0 items-center gap-4">
          <div className="flex flex-wrap justify-end gap-1">
            {workflow.triggers.map((trigger) => (
              <Badge
                key={trigger}
                variant="secondary"
                className="font-mono text-[10px]"
              >
                {trigger}
              </Badge>
            ))}
          </div>
          <span className="text-muted-foreground whitespace-nowrap font-mono text-xs tabular-nums">
            {workflow.job_count} jobs · {workflow.step_count} steps
          </span>
        </div>
      ) : null}
    </>
  )

  const className =
    "bg-card flex items-center justify-between gap-6 rounded-lg border p-4 text-sm"

  // A workflow that does not parse has no detail page to open.
  if (workflow.error) {
    return <div className={className}>{body}</div>
  }

  return (
    <Link
      to="/workflows/$id"
      params={{ id: workflow.id }}
      className={`${className} hover:border-muted-foreground/40 focus-visible:outline-ring transition-colors focus-visible:outline-2 focus-visible:outline-offset-2`}
    >
      {body}
    </Link>
  )
}

function ListSkeleton() {
  return (
    <div className="flex flex-col gap-2">
      {[0, 1, 2].map((index) => (
        <Skeleton key={index} className="h-[74px] w-full rounded-lg" />
      ))}
    </div>
  )
}

function Panel({
  icon: Icon,
  title,
  detail,
}: {
  icon: typeof AlertTriangle
  title: string
  detail: string
}) {
  return (
    <div className="bg-card flex items-start gap-3 rounded-lg border p-5">
      <Icon className="text-muted-foreground mt-0.5 size-4 shrink-0" aria-hidden />
      <div className="flex flex-col gap-1">
        <p className="text-sm font-medium">{title}</p>
        <p className="text-muted-foreground max-w-prose font-mono text-xs">
          {detail}
        </p>
      </div>
    </div>
  )
}
