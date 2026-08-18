import { useState } from "react"
import { Link, createFileRoute, useNavigate } from "@tanstack/react-router"
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { AlertTriangle, ArrowLeft, Play } from "lucide-react"

import { queries, runActions } from "@/lib/api"
import type { Job, Step, WorkflowDetail } from "@/lib/api"
import { DagCanvas } from "@/components/dag-canvas"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Skeleton } from "@/components/ui/skeleton"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"

export const Route = createFileRoute("/workflows/$id")({
  component: WorkflowDetailScreen,
})

function WorkflowDetailScreen() {
  const { id } = Route.useParams()
  const workflow = useQuery(queries.workflow(id))

  if (workflow.isPending) {
    return (
      <div className="flex flex-col gap-4 p-8">
        <Skeleton className="h-8 w-64" />
        <Skeleton className="h-52 w-full rounded-lg" />
      </div>
    )
  }

  if (workflow.isError) {
    return (
      <div className="flex flex-col items-start gap-4 p-8">
        <BackLink />
        <div className="bg-card flex items-start gap-3 rounded-lg border p-5">
          <AlertTriangle
            className="text-status-failure mt-0.5 size-4 shrink-0"
            aria-hidden
          />
          <div className="flex flex-col gap-1">
            <p className="text-sm font-medium">This workflow did not load</p>
            <p className="text-muted-foreground max-w-prose font-mono text-xs">
              {workflow.error.message}
            </p>
          </div>
        </div>
      </div>
    )
  }

  return <Loaded workflow={workflow.data} />
}

function Loaded({ workflow }: { workflow: WorkflowDetail }) {
  const [selectedJobId, setSelectedJobId] = useState(
    () => workflow.jobs[0]?.id ?? "",
  )
  const selectedJob =
    workflow.jobs.find((job) => job.id === selectedJobId) ?? workflow.jobs[0]

  return (
    <div className="flex flex-col gap-6 p-8">
      <header className="flex flex-col gap-3">
        <BackLink />
        <div className="flex flex-wrap items-center gap-3">
          <h1 className="font-mono text-xl font-bold tracking-tight">
            {workflow.name}
          </h1>
          {workflow.triggers.map((trigger) => (
            <Badge
              key={trigger}
              variant="secondary"
              className="font-mono text-[10px]"
            >
              {trigger}
            </Badge>
          ))}
          <span className="text-muted-foreground font-mono text-xs">
            {workflow.path}
          </span>

          <div className="ml-auto">
            <RunButton workflow={workflow} />
          </div>
        </div>
      </header>

      <section className="bg-card flex flex-col gap-2 rounded-lg border p-4">
        <div className="flex items-center justify-between">
          <h2 className="text-muted-foreground font-mono text-[10px] tracking-[0.14em] uppercase">
            Execution plan
          </h2>
          <span className="text-muted-foreground font-mono text-xs">
            {workflow.graph.error
              ? workflow.graph.error
              : `${workflow.graph.layers.length} layers · ${workflow.jobs.length} jobs`}
          </span>
        </div>

        {workflow.graph.error ? (
          <p className="text-status-failure font-mono text-xs">
            The scheduler could not order these jobs, so the graph below is
            unlayered.
          </p>
        ) : null}

        <DagCanvas
          layers={workflow.graph.layers}
          edges={workflow.graph.edges}
          nodes={workflow.jobs.map((job) => ({
            id: job.id,
            name: job.name,
            detail: `${job.steps.length} ${job.steps.length === 1 ? "step" : "steps"}${
              job.if ? " · conditional" : ""
            }`,
          }))}
          selectedId={selectedJob?.id}
          onSelect={setSelectedJobId}
        />
      </section>

      <Tabs defaultValue="steps" className="gap-4">
        <TabsList>
          <TabsTrigger value="steps">Steps</TabsTrigger>
          <TabsTrigger value="yaml">YAML</TabsTrigger>
          <TabsTrigger value="env">Environment</TabsTrigger>
        </TabsList>

        <TabsContent value="steps">
          {selectedJob ? (
            <JobSteps job={selectedJob} />
          ) : (
            <p className="text-muted-foreground text-sm">
              This workflow has no jobs.
            </p>
          )}
        </TabsContent>

        <TabsContent value="yaml">
          <pre className="bg-card overflow-x-auto rounded-lg border p-4 font-mono text-xs leading-relaxed">
            {workflow.yaml}
          </pre>
        </TabsContent>

        <TabsContent value="env">
          <EnvironmentTab workflow={workflow} />
        </TabsContent>
      </Tabs>
    </div>
  )
}

/** Starts a run and goes straight to it — the log is the point. */
function RunButton({ workflow }: { workflow: WorkflowDetail }) {
  const navigate = useNavigate()
  const queryClient = useQueryClient()

  // `workflow_dispatch` if the workflow accepts it, otherwise whatever it does
  // accept, so the run is not skipped by its own `on:` before it starts.
  const event = workflow.triggers.includes("workflow_dispatch")
    ? "workflow_dispatch"
    : (workflow.triggers[0] ?? "workflow_dispatch")

  const start = useMutation({
    mutationFn: () =>
      runActions.start({ workflow_id: workflow.id, event }),
    onSuccess: (run) => {
      queryClient.invalidateQueries({ queryKey: ["runs"] })
      navigate({ to: "/runs/$runId", params: { runId: run.id } })
    },
  })

  return (
    <div className="flex items-center gap-3">
      {start.isError ? (
        <span className="text-status-failure font-mono text-xs">
          {start.error.message}
        </span>
      ) : null}
      <Tooltip>
        <TooltipTrigger asChild>
          <Button
            size="sm"
            onClick={() => start.mutate()}
            disabled={start.isPending}
          >
            <Play className="size-3.5" aria-hidden />
            {start.isPending ? "Starting…" : "Run workflow"}
          </Button>
        </TooltipTrigger>
        <TooltipContent>Runs locally as `{event}`</TooltipContent>
      </Tooltip>
    </div>
  )
}

function JobSteps({ job }: { job: Job }) {
  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2">
        <h3 className="font-mono text-sm font-semibold">{job.name}</h3>
        <Badge variant="outline" className="font-mono text-[10px]">
          {job.id}
        </Badge>
        {job.needs.length > 0 ? (
          <span className="text-muted-foreground font-mono text-xs">
            needs {job.needs.join(", ")}
          </span>
        ) : null}
        {job.if ? (
          <span className="text-muted-foreground font-mono text-xs">
            if {job.if}
          </span>
        ) : null}
      </div>

      <ol className="flex flex-col gap-2">
        {job.steps.map((step) => (
          <li key={step.index}>
            <StepRow step={step} />
          </li>
        ))}
      </ol>
    </div>
  )
}

function StepRow({ step }: { step: Step }) {
  const withEntries = Object.entries(step.with)

  return (
    <div className="bg-card flex gap-3 rounded-lg border p-3">
      <span className="text-muted-foreground w-6 shrink-0 text-right font-mono text-xs tabular-nums">
        {step.index + 1}
      </span>

      <div className="flex min-w-0 flex-1 flex-col gap-2">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-sm font-medium">{step.name}</span>
          {step.id ? (
            <Badge variant="outline" className="font-mono text-[10px]">
              id: {step.id}
            </Badge>
          ) : null}
          {step.continue_on_error ? (
            <Badge variant="secondary" className="text-[10px]">
              continue-on-error
            </Badge>
          ) : null}
        </div>

        {step.uses ? (
          <code className="text-primary font-mono text-xs">
            uses: {step.uses}
          </code>
        ) : null}

        {step.run ? (
          <pre className="bg-muted/40 overflow-x-auto rounded-md p-2 font-mono text-xs leading-relaxed">
            {step.run.trimEnd()}
          </pre>
        ) : null}

        {withEntries.length > 0 ? (
          <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 font-mono text-xs">
            {withEntries.map(([key, value]) => (
              <div key={key} className="contents">
                <dt className="text-muted-foreground">with.{key}</dt>
                <dd className="truncate">{value}</dd>
              </div>
            ))}
          </dl>
        ) : null}

        <div className="text-muted-foreground flex flex-wrap gap-x-4 font-mono text-[11px]">
          {step.if ? <span>if: {step.if}</span> : null}
          {step.shell ? <span>shell: {step.shell}</span> : null}
          {step.working_directory ? (
            <span>cwd: {step.working_directory}</span>
          ) : null}
        </div>
      </div>
    </div>
  )
}

function EnvironmentTab({ workflow }: { workflow: WorkflowDetail }) {
  const env = Object.entries(workflow.env)

  return (
    <div className="flex flex-col gap-6">
      <section className="flex flex-col gap-2">
        <h3 className="text-muted-foreground font-mono text-[10px] tracking-[0.14em] uppercase">
          Workflow env
        </h3>
        {env.length === 0 ? (
          <p className="text-muted-foreground text-sm">
            No workflow-level environment variables.
          </p>
        ) : (
          <dl className="bg-card grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 rounded-lg border p-4 font-mono text-xs">
            {env.map(([key, value]) => (
              <div key={key} className="contents">
                <dt className="text-primary">{key}</dt>
                <dd className="break-all">{value}</dd>
              </div>
            ))}
          </dl>
        )}
      </section>

      <section className="flex flex-col gap-2">
        <h3 className="text-muted-foreground font-mono text-[10px] tracking-[0.14em] uppercase">
          workflow_dispatch inputs
        </h3>
        {workflow.dispatch_inputs.length === 0 ? (
          <p className="text-muted-foreground text-sm">
            This workflow takes no manual inputs.
          </p>
        ) : (
          <ul className="bg-card flex flex-col gap-3 rounded-lg border p-4">
            {workflow.dispatch_inputs.map((input) => (
              <li key={input.name} className="flex flex-col gap-0.5">
                <div className="flex items-center gap-2">
                  <span className="text-primary font-mono text-xs">
                    {input.name}
                  </span>
                  {input.required ? (
                    <Badge variant="secondary" className="text-[10px]">
                      required
                    </Badge>
                  ) : null}
                  {input.default ? (
                    <span className="text-muted-foreground font-mono text-[11px]">
                      default: {input.default}
                    </span>
                  ) : null}
                </div>
                {input.description ? (
                  <span className="text-muted-foreground text-xs">
                    {input.description}
                  </span>
                ) : null}
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  )
}

function BackLink() {
  return (
    <Link
      to="/workflows"
      className="text-muted-foreground hover:text-foreground flex w-fit items-center gap-1.5 text-xs transition-colors"
    >
      <ArrowLeft className="size-3.5" aria-hidden />
      All workflows
    </Link>
  )
}
