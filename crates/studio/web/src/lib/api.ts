/**
 * Client for the Rust API.
 *
 * Studio is a SPA served by the `minact` binary, so every request is a plain
 * same-origin fetch — there are no server functions and no SSR to account for.
 * In development Vite proxies `/api` to the Rust server on port 4000.
 */
import { queryOptions } from "@tanstack/react-query"

export type Meta = {
  version: string
  workspace: string
  runner: { os: string; arch: string }
  actions: Array<string>
  workflow_count: number
}

export type WorkflowSummary = {
  id: string
  path: string
  source: string
  name: string
  triggers: Array<string>
  job_count: number
  step_count: number
  /** Present when the file failed to parse; all counts are 0 then. */
  error?: string
}

export type Step = {
  index: number
  id?: string
  name: string
  uses?: string
  run?: string
  shell?: string
  working_directory?: string
  if?: string
  continue_on_error?: boolean
  with: Record<string, string>
  env: Record<string, string>
}

export type Job = {
  id: string
  name: string
  needs: Array<string>
  if?: string
  runs_on?: string
  env: Record<string, string>
  steps: Array<Step>
}

export type Graph = {
  /** Job ids grouped into the layers the scheduler resolved. */
  layers: Array<Array<string>>
  edges: Array<{ from: string; to: string }>
  /** Set when the DAG has a cycle, in which case `layers` is empty. */
  error?: string
}

export type DispatchInput = {
  name: string
  description: string
  required: boolean
  default?: string
  type?: string
}

export type WorkflowDetail = WorkflowSummary & {
  yaml: string
  env: Record<string, string>
  jobs: Array<Job>
  graph: Graph
  dispatch_inputs: Array<DispatchInput>
}

/** An error the API reported, carrying the message the server wrote. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message)
    this.name = "ApiError"
  }
}

async function get<T>(path: string): Promise<T> {
  const response = await fetch(`/api${path}`, {
    headers: { accept: "application/json" },
  })

  if (!response.ok) {
    // Errors are JSON too, but a crashed or misrouted server may return HTML.
    const body = await response.json().catch(() => null)
    throw new ApiError(
      response.status,
      body?.code ?? "unknown",
      body?.message ?? `${response.status} ${response.statusText}`,
    )
  }

  return response.json() as Promise<T>
}

export const queries = {
  meta: () =>
    queryOptions({
      queryKey: ["meta"],
      queryFn: () => get<Meta>("/meta"),
    }),
  workflows: () =>
    queryOptions({
      queryKey: ["workflows"],
      queryFn: () => get<Array<WorkflowSummary>>("/workflows"),
    }),
  workflow: (id: string) =>
    queryOptions({
      queryKey: ["workflows", id],
      queryFn: () => get<WorkflowDetail>(`/workflows/${id}`),
      retry: (count: number, error: Error) =>
        // A workflow that does not parse will never parse on a retry.
        error instanceof ApiError && error.status < 500 ? false : count < 2,
    }),
}

// ── Runs ──────────────────────────────────────────────────────────────────

export type RunStatus =
  | "running"
  | "success"
  | "failure"
  | "cancelled"
  | "errored"

export type Conclusion = "success" | "failure" | "cancelled" | "skipped"

export type RunSummary = {
  id: string
  workflow_id: string
  workflow_name: string
  workflow_path: string
  event: string
  inputs: Record<string, string>
  status: RunStatus
  started_at: string
  finished_at?: string
  duration_ms?: number
  error?: string
}

export type RunStepView = {
  index: number
  name: string
  /** `null` while the step is still running. */
  conclusion: Conclusion | null
  started_at?: string
  duration_ms?: number
  note?: string
}

export type RunJobView = {
  /** The job *instance* id — `build (os=macos)` under a matrix. */
  id: string
  name: string
  conclusion: Conclusion | null
  started_at?: string
  duration_ms?: number
  note?: string
  steps: Array<RunStepView>
}

export type RunDetail = RunSummary & {
  layers: Array<Array<string>>
  jobs: Array<RunJobView>
  /** Highest sequence number folded in; where a live stream resumes from. */
  last_seq: number | null
}

/** The engine's event envelope, exactly as it comes over SSE. */
export type LogRecord = {
  seq: number
  ts: string
  scope: { job_id?: string; step_index?: number }
  event: LogEvent
}

export type LogEvent =
  | { type: "workflow_started"; workflow_name: string; event_name: string }
  | { type: "execution_plan"; layers: Array<Array<string>> }
  | { type: "job_started"; job_id: string; job_name: string }
  | { type: "job_skipped"; job_id: string; job_name: string; condition: string }
  | { type: "job_cancelled"; job_id: string; job_name: string; reason: string }
  | {
      type: "job_finished"
      job_id: string
      job_name: string
      success: boolean
      conclusion: Conclusion
    }
  | {
      type: "step_started"
      job_id: string
      step_index: number
      step_name: string
    }
  | {
      type: "step_skipped"
      job_id: string
      step_index: number
      step_name: string
      condition: string
    }
  | { type: "action_started"; uses: string }
  | { type: "action_input"; name: string; value: string }
  | { type: "action_finished"; success: boolean; conclusion: Conclusion }
  | { type: "action_error"; message: string }
  | {
      type: "command_started"
      command: string
      shell: string
      working_dir: string
    }
  | { type: "command_output"; stream: "stdout" | "stderr"; line: string }
  | { type: "command_finished"; success: boolean; status: string }
  | { type: "message"; level: "info" | "warn" | "error"; message: string }

async function send<T>(path: string, body?: unknown): Promise<T> {
  const response = await fetch(`/api${path}`, {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(body ?? {}),
  })

  if (!response.ok) {
    const failure = await response.json().catch(() => null)
    throw new ApiError(
      response.status,
      failure?.code ?? "unknown",
      failure?.message ?? `${response.status} ${response.statusText}`,
    )
  }

  return response.json() as Promise<T>
}

export const runQueries = {
  list: () =>
    queryOptions({
      queryKey: ["runs"],
      queryFn: () => get<Array<RunSummary>>("/runs"),
    }),
  detail: (id: string) =>
    queryOptions({
      queryKey: ["runs", id],
      queryFn: () => get<RunDetail>(`/runs/${id}`),
    }),
}

export const runActions = {
  start: (input: {
    workflow_id: string
    event?: string
    inputs?: Record<string, string>
  }) => send<RunSummary>("/runs", input),
  cancel: (id: string) => send<RunSummary>(`/runs/${id}/cancel`),
}

// ── Artifacts ─────────────────────────────────────────────────────────────

export type ArtifactFile = {
  /** Path relative to the artifact's own directory. */
  path: string
  bytes: number
  /** Small enough and textual enough to show inline. */
  previewable: boolean
}

export type Artifact = {
  name: string
  file_count: number
  total_bytes: number
  modified?: string
  files: Array<ArtifactFile>
}

export const artifactQueries = {
  list: () =>
    queryOptions({
      queryKey: ["artifacts"],
      queryFn: () => get<Array<Artifact>>("/artifacts"),
    }),
}

/** The URL a browser can fetch or download an artifact file from. */
export function artifactFileUrl(name: string, path: string) {
  const segments = path.split("/").map(encodeURIComponent).join("/")
  return `/api/artifacts/${encodeURIComponent(name)}/${segments}`
}

/** The URL that downloads a run's log; `job` narrows it to one job. */
export function runLogUrl(runId: string, job?: string) {
  const query = job ? `?job=${encodeURIComponent(job)}` : ""
  return `/api/runs/${encodeURIComponent(runId)}/logs${query}`
}

/** Runs, optionally filtered. Filters live in the URL, so the key includes them. */
export function runListQuery(filters: {
  workflow?: string
  status?: RunStatus
}) {
  const params = new URLSearchParams()
  if (filters.workflow) params.set("workflow", filters.workflow)
  if (filters.status) params.set("status", filters.status)
  const query = params.toString()

  return queryOptions({
    queryKey: ["runs", "list", filters],
    queryFn: () => get<Array<RunSummary>>(`/runs${query ? `?${query}` : ""}`),
  })
}
