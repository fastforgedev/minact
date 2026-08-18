/**
 * The live event feed for one run.
 *
 * Log lines live here rather than in React state on purpose. A noisy build
 * emits hundreds of lines a second, and putting each one through `setState`
 * would re-render the whole screen that many times. Instead the stream owns a
 * plain array, coalesces arrivals into one notification per animation frame,
 * and components read it through `useSyncExternalStore`.
 */
import type { Conclusion, LogRecord, RunStatus } from "@/lib/api"

/** One rendered row in the log viewer. */
export type LogLine = {
  /** Unique and stable: a record can expand to several lines. */
  key: string
  seq: number
  ts: string
  jobId?: string
  stepIndex?: number
  kind:
    | "job"
    | "step"
    | "command"
    | "stdout"
    | "stderr"
    | "info"
    | "warn"
    | "error"
  text: string
}

export type StreamState = "connecting" | "live" | "ended" | "error"

/**
 * Turn one event into the lines it should show as.
 *
 * Every row is exactly one line: rows are fixed-height and virtualized, so a
 * `run: |` block arriving as one string would paint over its neighbours.
 */
function toLines(record: LogRecord): Array<LogLine> {
  const base = {
    seq: record.seq,
    ts: record.ts,
    jobId: record.scope.job_id,
    stepIndex: record.scope.step_index,
  }

  const line = (kind: LogLine["kind"], text: string, suffix = ""): Array<LogLine> =>
    // A `run: |` block ends with a newline; that is YAML block syntax, not a
    // blank line the user wrote.
    text
      .replace(/\n+$/, "")
      .split("\n")
      .map((part, index) => ({
      ...base,
      kind,
        text: part,
        key: `${record.seq}${suffix}:${index}`,
      }))

  const event = record.event
  switch (event.type) {
    case "job_started":
      return line("job", `job ${event.job_name}`)
    case "job_skipped":
      return line("info", `job ${event.job_name} skipped (${event.condition})`)
    case "job_cancelled":
      return line("warn", `job ${event.job_name} cancelled (${event.reason})`)
    case "step_started":
      return line("step", event.step_name)
    case "step_skipped":
      return line("info", `${event.step_name} skipped (${event.condition})`)
    case "action_started":
      return line("command", `uses: ${event.uses}`)
    case "action_input":
      return line("info", `  with ${event.name} = ${event.value}`)
    case "action_error":
      return line("error", event.message)
    case "command_started":
      // A `run: |` block is many lines; the first carries the prompt.
      return line("command", event.command)
        .map((row, index) => ({
          ...row,
          text: index === 0 ? `$ ${row.text}` : `  ${row.text}`,
        }))
    case "command_output":
      return line(event.stream === "stderr" ? "stderr" : "stdout", event.line)
    case "command_finished":
      return event.success
        ? []
        : line("error", `command failed: ${event.status}`)
    case "message":
      return line(event.level, event.message)
    default:
      return []
  }
}

export class RunStream {
  lines: Array<LogLine> = []
  /** Conclusions seen so far, so the DAG can colour without a refetch. */
  jobConclusions = new Map<string, Conclusion>()
  state: StreamState = "connecting"
  finalStatus: RunStatus | null = null
  lastSeq = -1
  /** Bumped on every flush; this is the `useSyncExternalStore` snapshot. */
  version = 0

  private source: EventSource | null = null
  private listeners = new Set<() => void>()
  private pending: Array<LogLine> = []
  private frame: number | null = null

  constructor(private readonly runId: string) {}

  subscribe = (listener: () => void) => {
    this.listeners.add(listener)
    if (!this.source) this.open()
    return () => {
      this.listeners.delete(listener)
    }
  }

  getSnapshot = () => this.version

  close() {
    this.source?.close()
    this.source = null
    if (this.frame !== null) cancelAnimationFrame(this.frame)
    this.frame = null
  }

  private open() {
    // Resume rather than restart: `from` is exclusive of what we already hold,
    // so a reconnect after a dropped connection leaves no gap and no duplicate.
    const from = this.lastSeq + 1
    const source = new EventSource(
      `/api/runs/${this.runId}/events?from=${from}`,
    )
    this.source = source

    source.addEventListener("record", (message) => {
      const record = JSON.parse((message as MessageEvent).data) as LogRecord
      if (record.seq <= this.lastSeq) return
      this.lastSeq = record.seq

      if (record.event.type === "job_finished") {
        this.jobConclusions.set(record.event.job_id, record.event.conclusion)
      }
      if (record.event.type === "job_skipped") {
        this.jobConclusions.set(record.event.job_id, "skipped")
      }
      if (record.event.type === "job_cancelled") {
        this.jobConclusions.set(record.event.job_id, "cancelled")
      }

      this.state = "live"
      this.pending.push(...toLines(record))
      this.schedule()
    })

    source.addEventListener("end", (message) => {
      this.finalStatus = JSON.parse((message as MessageEvent).data) as RunStatus
      this.state = "ended"
      // The server closes the stream itself; closing here stops the browser
      // from immediately reconnecting to a finished run.
      this.close()
      this.flush()
    })

    source.addEventListener("lagged", () => {
      // We fell behind the server's buffer. Reconnecting replays the gap from
      // `lastSeq`, which is exactly what the resume parameter is for.
      this.source?.close()
      this.source = null
      this.open()
    })

    source.onerror = () => {
      // EventSource reconnects on its own, but it would resume from the
      // original URL. Close and reopen so `from` reflects what we have.
      if (this.state === "ended") return
      this.state = "error"
      this.flush()
      this.source?.close()
      this.source = null
      window.setTimeout(() => {
        if (this.listeners.size > 0 && this.state !== "ended") this.open()
      }, 1000)
    }
  }

  private schedule() {
    if (this.frame !== null) return
    this.frame = requestAnimationFrame(() => {
      this.frame = null
      this.flush()
    })
  }

  private flush() {
    if (this.pending.length > 0) {
      this.lines = this.lines.concat(this.pending)
      this.pending = []
    }
    this.version += 1
    for (const listener of this.listeners) listener()
  }
}
