/**
 * Status vocabulary shared by every screen.
 *
 * Conclusions are encoded in shape as well as colour — a dot plus a word — so
 * they survive being scanned quickly and do not rely on colour alone.
 */
import type { Conclusion, RunStatus } from "@/lib/api"
import { cn } from "@/lib/utils"

const RUN_TONE: Record<RunStatus, string> = {
  running: "text-status-running border-status-running/40 bg-status-running/10",
  success: "text-status-success border-status-success/40 bg-status-success/10",
  failure: "text-status-failure border-status-failure/40 bg-status-failure/10",
  cancelled: "text-muted-foreground border-border bg-muted/40",
  errored: "text-status-failure border-status-failure/40 bg-status-failure/10",
}

export function RunStatusPill({
  status,
  className,
}: {
  status: RunStatus
  className?: string
}) {
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 rounded-full border px-2.5 py-0.5 font-mono text-[11px]",
        RUN_TONE[status],
        className,
      )}
    >
      <span
        className={cn(
          "size-1.5 rounded-full bg-current",
          status === "running" && "motion-safe:animate-pulse",
        )}
        aria-hidden
      />
      {status}
    </span>
  )
}

const CONCLUSION_TONE: Record<Conclusion, string> = {
  success: "text-status-success",
  failure: "text-status-failure",
  cancelled: "text-status-failure/70",
  skipped: "text-status-skipped",
}

const CONCLUSION_MARK: Record<Conclusion, string> = {
  success: "✓",
  failure: "✗",
  cancelled: "⊘",
  skipped: "–",
}

/** A conclusion, or a pulsing dot while it is still running. */
export function ConclusionMark({
  conclusion,
  running,
  className,
}: {
  conclusion: Conclusion | null
  running?: boolean
  className?: string
}) {
  if (conclusion === null) {
    return (
      <span
        className={cn(
          "text-status-running inline-block w-3 text-center",
          running && "motion-safe:animate-pulse",
          className,
        )}
        title={running ? "running" : "pending"}
        aria-label={running ? "running" : "pending"}
      >
        {running ? "▸" : "·"}
      </span>
    )
  }

  return (
    <span
      className={cn(
        "inline-block w-3 text-center",
        CONCLUSION_TONE[conclusion],
        className,
      )}
      title={conclusion}
      aria-label={conclusion}
    >
      {CONCLUSION_MARK[conclusion]}
    </span>
  )
}

/** `1.4s`, `2m 05s` — durations read at a glance, not to the millisecond. */
export function formatDuration(ms: number | null | undefined) {
  if (ms === null || ms === undefined) return "—"
  if (ms < 1000) return `${ms}ms`

  const totalSeconds = ms / 1000
  if (totalSeconds < 60) return `${totalSeconds.toFixed(1)}s`

  const minutes = Math.floor(totalSeconds / 60)
  const seconds = Math.floor(totalSeconds % 60)
  return `${minutes}m ${String(seconds).padStart(2, "0")}s`
}

/** How long ago, in the coarsest unit that is still informative. */
export function formatRelative(iso: string) {
  const seconds = Math.max(0, (Date.now() - new Date(iso).getTime()) / 1000)
  if (seconds < 60) return "just now"
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`
  if (seconds < 86_400) return `${Math.floor(seconds / 3600)}h ago`
  return `${Math.floor(seconds / 86_400)}d ago`
}
