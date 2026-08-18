import {
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
} from "react"
import { Link, createFileRoute } from "@tanstack/react-router"
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { AlertTriangle, ArrowLeft, Download, Square } from "lucide-react"

import { queries, runActions, runLogUrl, runQueries } from "@/lib/api"
import type { RunDetail, RunJobView } from "@/lib/api"
import { RunStream } from "@/lib/run-stream"
import { DagCanvas } from "@/components/dag-canvas"
import type { DagNode } from "@/components/dag-canvas"
import { LogStream } from "@/components/log-stream"
import {
  ConclusionMark,
  RunStatusPill,
  formatDuration,
} from "@/components/status"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Skeleton } from "@/components/ui/skeleton"
import { cn } from "@/lib/utils"

export const Route = createFileRoute("/runs/$runId")({
  component: RunScreen,
})

function RunScreen() {
  const { runId } = Route.useParams()
  const queryClient = useQueryClient()

  // One stream per run id, torn down when the screen leaves.
  const stream = useMemo(() => new RunStream(runId), [runId])
  useEffect(() => () => stream.close(), [stream])

  // Log lines live outside React; this subscribes to the stream's flushes.
  useSyncExternalStore(stream.subscribe, stream.getSnapshot, () => 0)

  const run = useQuery({
    ...runQueries.detail(runId),
    // The stream carries the events; the query carries the folded-up state.
    // Poll only while the run is live, and stop the moment it ends.
    refetchInterval: (query) =>
      query.state.data?.status === "running" ? 1500 : false,
  })

  // The stream sees the run end before the next poll would. Fire once: this
  // effect re-runs on every flush, and invalidating per frame would refetch
  // the run a hundred times a second.
  const settled = useRef(false)
  useEffect(() => {
    if (stream.state === "ended" && !settled.current) {
      settled.current = true
      queryClient.invalidateQueries({ queryKey: ["runs"] })
    }
  }, [stream.state, queryClient])

  const cancel = useMutation({
    mutationFn: () => runActions.cancel(runId),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["runs", runId] }),
  })

  if (run.isPending) {
    return (
      <div className="flex flex-col gap-4 p-8">
        <Skeleton className="h-8 w-64" />
        <Skeleton className="h-40 w-full rounded-lg" />
      </div>
    )
  }

  if (run.isError) {
    return (
      <div className="flex flex-col items-start gap-4 p-8">
        <BackLink />
        <div className="bg-card flex items-start gap-3 rounded-lg border p-5">
          <AlertTriangle
            className="text-status-failure mt-0.5 size-4 shrink-0"
            aria-hidden
          />
          <p className="text-muted-foreground font-mono text-xs">
            {run.error.message}
          </p>
        </div>
      </div>
    )
  }

  return (
    <Loaded
      run={run.data}
      stream={stream}
      onCancel={() => cancel.mutate()}
      cancelling={cancel.isPending}
    />
  )
}

function Loaded({
  run,
  stream,
  onCancel,
  cancelling,
}: {
  run: RunDetail
  stream: RunStream
  onCancel: () => void
  cancelling: boolean
}) {
  const [selectedJobId, setSelectedJobId] = useState<string | undefined>()

  // Default to whatever is running, then to whatever went wrong — the job you
  // would have clicked anyway.
  const focusJobId =
    selectedJobId ??
    run.jobs.find((job) => job.conclusion === null)?.id ??
    run.jobs.find((job) => job.conclusion === "failure")?.id ??
    run.jobs.find((job) => job.conclusion === "cancelled")?.id ??
    run.jobs[0]?.id

  const selectedJob = run.jobs.find((job) => job.id === focusJobId)
  const live = run.status === "running"

  const nodes = useMemo(
    () => buildNodes(run, stream, live),
    // The stream mutates in place, so its version is what marks it changed.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [run, stream, stream.version, live],
  )

  // A run records the layers it executed but not the `needs:` edges, so take
  // them from the workflow. If the file has changed or gone since, the layer
  // columns still show the order — drawing guessed edges would be worse.
  const workflow = useQuery({
    ...queries.workflow(run.workflow_id),
    retry: false,
  })
  const edges = useMemo(
    () => expandEdges(workflow.data?.graph.edges ?? [], run.jobs),
    [workflow.data, run.jobs],
  )

  return (
    <div className="flex h-full min-h-0 flex-col">
      <header className="flex flex-col gap-3 border-b px-8 py-5">
        <BackLink />
        <div className="flex flex-wrap items-center gap-3">
          <h1 className="font-mono text-xl font-bold tracking-tight">
            {run.workflow_name}
          </h1>
          <RunStatusPill status={run.status} />
          <span className="text-muted-foreground font-mono text-xs">
            run #{run.id}
          </span>
          <Badge variant="secondary" className="font-mono text-[10px]">
            {run.event}
          </Badge>
          <span className="text-muted-foreground font-mono text-xs tabular-nums">
            {formatDuration(run.duration_ms)}
          </span>

          <div className="ml-auto flex items-center gap-3">
            {/* The server sets `Content-Disposition`, so a plain link is the
                whole download — no blob, no script. */}
            <a
              href={runLogUrl(run.id)}
              download
              className="text-muted-foreground hover:text-foreground focus-visible:outline-ring flex items-center gap-1.5 rounded text-xs transition-colors focus-visible:outline-2 focus-visible:outline-offset-2"
            >
              <Download className="size-3.5" aria-hidden />
              Log
            </a>

            {live ? (
              <Button
                size="sm"
                variant="outline"
                onClick={onCancel}
                disabled={cancelling}
                className="text-status-failure"
              >
                <Square className="size-3.5" aria-hidden />
                {cancelling ? "Cancelling…" : "Cancel"}
              </Button>
            ) : (
              <Link
                to="/workflows/$id"
                params={{ id: run.workflow_id }}
                className="text-muted-foreground hover:text-foreground text-xs transition-colors"
              >
                Open workflow
              </Link>
            )}
          </div>
        </div>

        {run.error ? (
          <p className="text-status-failure font-mono text-xs">{run.error}</p>
        ) : null}
      </header>

      <section className="border-b px-8 py-4">
        <h2 className="text-muted-foreground mb-2 font-mono text-[10px] tracking-[0.14em] uppercase">
          Execution plan
        </h2>
        <DagCanvas
          layers={layersWithInstances(run)}
          edges={edges}
          nodes={nodes}
          selectedId={focusJobId}
          onSelect={setSelectedJobId}
        />
      </section>

      <div className="flex min-h-0 flex-1">
        <aside className="w-72 shrink-0 overflow-y-auto border-r">
          {run.jobs.map((job) => (
            <JobSteps
              key={job.id}
              job={job}
              selected={job.id === focusJobId}
              onSelect={() => setSelectedJobId(job.id)}
              live={live}
            />
          ))}
          {run.jobs.length === 0 ? (
            <p className="text-muted-foreground p-4 font-mono text-xs">
              Waiting for the first job…
            </p>
          ) : null}
        </aside>

        <LogStream
          stream={stream}
          lines={stream.lines}
          jobId={selectedJob?.id}
          className="min-w-0 flex-1"
        />
      </div>
    </div>
  )
}

function JobSteps({
  job,
  selected,
  onSelect,
  live,
}: {
  job: RunJobView
  selected: boolean
  onSelect: () => void
  live: boolean
}) {
  return (
    <div className={cn("border-b", selected && "bg-accent/40")}>
      <button
        type="button"
        onClick={onSelect}
        aria-pressed={selected}
        className="hover:bg-accent/60 focus-visible:outline-ring flex w-full items-center gap-2 px-3 py-2 text-left focus-visible:outline-2 focus-visible:-outline-offset-2"
      >
        <ConclusionMark
          conclusion={job.conclusion}
          running={live && job.started_at !== undefined}
        />
        <span className="min-w-0 flex-1 truncate font-mono text-xs font-semibold">
          {job.name}
        </span>
        <span className="text-muted-foreground font-mono text-[11px] tabular-nums">
          {formatDuration(job.duration_ms)}
        </span>
      </button>

      {job.note ? (
        <p className="text-muted-foreground px-3 pb-2 font-mono text-[11px]">
          {job.note}
        </p>
      ) : null}

      <ol className="pb-2">
        {job.steps.map((step) => (
          <li
            key={step.index}
            className="flex items-center gap-2 px-3 py-1 pl-6"
            title={step.note}
          >
            <ConclusionMark
              conclusion={step.conclusion}
              running={live && step.started_at !== undefined}
            />
            <span className="text-muted-foreground min-w-0 flex-1 truncate text-xs">
              {step.name}
            </span>
            <span className="text-muted-foreground/70 font-mono text-[10.5px] tabular-nums">
              {step.duration_ms === undefined
                ? ""
                : formatDuration(step.duration_ms)}
            </span>
          </li>
        ))}
      </ol>
    </div>
  )
}

/**
 * Nodes for the graph: every job in the plan, whether or not it has started.
 *
 * A job only appears in `run.jobs` once it emits an event, so building nodes
 * from those alone would grow the graph as the run went — the plan is known up
 * front and should be drawn that way.
 */
function buildNodes(
  run: RunDetail,
  stream: RunStream,
  live: boolean,
): Array<DagNode> {
  const fromJob = (job: RunJobView): DagNode => ({
    id: job.id,
    name: job.name,
    detail: jobDetail(job),
    // The stream knows a conclusion before the next poll folds it in.
    conclusion: stream.jobConclusions.get(job.id) ?? job.conclusion,
    running: job.conclusion === null && job.started_at !== undefined && live,
  })

  const nodes: Array<DagNode> = []
  const placed = new Set<string>()

  for (const plannedId of run.layers.flat()) {
    // A matrix job is one id in the plan and several instances in the run.
    const instances = run.jobs.filter(
      (job) => job.id === plannedId || job.id.startsWith(`${plannedId} (`),
    )

    if (instances.length === 0) {
      nodes.push({ id: plannedId, name: plannedId, detail: "queued" })
      placed.add(plannedId)
      continue
    }
    for (const job of instances) {
      nodes.push(fromJob(job))
      placed.add(job.id)
    }
  }

  // A run of a workflow that has since changed can hold jobs the current plan
  // does not; show them rather than dropping them.
  for (const job of run.jobs) {
    if (!placed.has(job.id)) nodes.push(fromJob(job))
  }

  return nodes
}

function jobDetail(job: RunJobView) {
  if (job.conclusion === null && job.started_at) {
    const done = job.steps.filter((step) => step.conclusion !== null).length
    return `step ${done + 1}/${job.steps.length || "?"}`
  }
  if (job.duration_ms !== undefined) {
    const count = job.steps.length
    return `${count} ${count === 1 ? "step" : "steps"} · ${formatDuration(job.duration_ms)}`
  }
  return job.note ?? "queued"
}

/** Expand each planned layer to the instances that actually ran. */
function layersWithInstances(run: RunDetail) {
  return run.layers.map((layer) =>
    layer.flatMap((plannedId) => {
      const instances = run.jobs
        .map((job) => job.id)
        .filter((id) => id === plannedId || id.startsWith(`${plannedId} (`))
      return instances.length > 0 ? instances : [plannedId]
    }),
  )
}

/**
 * Map the workflow's job-to-job edges onto the run's job *instances*.
 *
 * A matrix job appears once in `needs:` and many times in a run, so one edge
 * becomes one per instance pair.
 */
function expandEdges(
  edges: Array<{ from: string; to: string }>,
  jobs: Array<RunJobView>,
) {
  const instancesOf = (baseId: string) =>
    jobs
      .map((job) => job.id)
      .filter((id) => id === baseId || id.startsWith(`${baseId} (`))

  return edges.flatMap((edge) =>
    instancesOf(edge.from).flatMap((from) =>
      instancesOf(edge.to).map((to) => ({ from, to })),
    ),
  )
}

function BackLink() {
  return (
    <Link
      to="/runs"
      className="text-muted-foreground hover:text-foreground flex w-fit items-center gap-1.5 text-xs transition-colors"
    >
      <ArrowLeft className="size-3.5" aria-hidden />
      All runs
    </Link>
  )
}
