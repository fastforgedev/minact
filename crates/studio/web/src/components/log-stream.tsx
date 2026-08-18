/**
 * The log viewer.
 *
 * Virtualized because a real build produces tens of thousands of lines, and
 * subscribed through `useSyncExternalStore` so incoming output re-renders this
 * component and nothing else.
 */
import { useDeferredValue, useEffect, useMemo, useRef, useState } from "react"
import { useVirtualizer } from "@tanstack/react-virtual"
import { ArrowDownToLine, Search } from "lucide-react"

import type { LogLine, RunStream } from "@/lib/run-stream"
import { cn } from "@/lib/utils"
import { Input } from "@/components/ui/input"
import { Button } from "@/components/ui/button"

const ROW_HEIGHT = 20

type Props = {
  stream: RunStream
  lines: Array<LogLine>
  /** Show only this job's output; `undefined` shows the whole run. */
  jobId?: string
  className?: string
}

export function LogStream({ stream, lines, jobId, className }: Props) {
  const [filter, setFilter] = useState("")
  const [errorsOnly, setErrorsOnly] = useState(false)
  const [follow, setFollow] = useState(true)
  const scrollRef = useRef<HTMLDivElement>(null)

  // Typing must not block new output from painting.
  const deferredFilter = useDeferredValue(filter)

  const visible = useMemo(() => {
    const needle = deferredFilter.trim().toLowerCase()
    return lines.filter((line) => {
      if (jobId && line.jobId !== jobId) return false
      if (errorsOnly && line.kind !== "stderr" && line.kind !== "error") {
        return false
      }
      if (needle && !line.text.toLowerCase().includes(needle)) return false
      return true
    })
  }, [lines, jobId, errorsOnly, deferredFilter])

  const virtualizer = useVirtualizer({
    count: visible.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 24,
  })

  useEffect(() => {
    if (follow && visible.length > 0) {
      virtualizer.scrollToIndex(visible.length - 1, { align: "end" })
    }
  }, [follow, visible.length, virtualizer])

  const items = virtualizer.getVirtualItems()

  return (
    <div className={cn("flex min-h-0 flex-col", className)}>
      <div className="flex items-center gap-2 border-b px-3 py-2">
        <div className="relative">
          <Search
            className="text-muted-foreground pointer-events-none absolute top-1/2 left-2 size-3.5 -translate-y-1/2"
            aria-hidden
          />
          <Input
            value={filter}
            onChange={(event) => setFilter(event.target.value)}
            placeholder="Filter lines"
            aria-label="Filter log lines"
            className="h-7 w-56 pl-7 font-mono text-xs"
          />
        </div>

        <Button
          variant={errorsOnly ? "secondary" : "ghost"}
          size="sm"
          className="h-7 text-xs"
          aria-pressed={errorsOnly}
          onClick={() => setErrorsOnly((on) => !on)}
        >
          Errors
        </Button>

        <span className="text-muted-foreground font-mono text-[11px] tabular-nums">
          {visible.length === lines.length
            ? `${lines.length} lines`
            : `${visible.length} of ${lines.length}`}
        </span>

        <div className="flex-1" />

        <span
          className={cn(
            "font-mono text-[11px]",
            stream.state === "live" && "text-status-running",
            stream.state === "error" && "text-status-failure",
            stream.state !== "live" &&
              stream.state !== "error" &&
              "text-muted-foreground",
          )}
        >
          {streamLabel(stream.state)}
        </span>

        <Button
          variant={follow ? "secondary" : "ghost"}
          size="sm"
          className="h-7 text-xs"
          aria-pressed={follow}
          onClick={() => setFollow((on) => !on)}
        >
          <ArrowDownToLine className="size-3.5" aria-hidden />
          Tail
        </Button>
      </div>

      <div
        ref={scrollRef}
        // Scrolling up is how you stop following; scrolling back to the bottom
        // resumes, which is what a terminal does.
        onScroll={(event) => {
          const el = event.currentTarget
          const atBottom =
            el.scrollHeight - el.scrollTop - el.clientHeight < ROW_HEIGHT * 2
          if (atBottom !== follow) setFollow(atBottom)
        }}
        className="min-h-0 flex-1 overflow-auto"
      >
        {visible.length === 0 ? (
          <p className="text-muted-foreground p-4 font-mono text-xs">
            {lines.length === 0
              ? "Waiting for output…"
              : "No lines match that filter."}
          </p>
        ) : (
          <div
            style={{ height: virtualizer.getTotalSize() }}
            className="relative w-full"
          >
            {items.map((item) => (
              <Row
                key={visible[item.index].key}
                line={visible[item.index]}
                top={item.start}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  )
}

function streamLabel(state: RunStream["state"]) {
  switch (state) {
    case "connecting":
      return "connecting…"
    case "live":
      return "● live"
    case "error":
      return "reconnecting…"
    case "ended":
      return "finished"
  }
}

const TONE: Record<LogLine["kind"], string> = {
  job: "text-foreground font-semibold",
  step: "text-primary font-medium",
  command: "text-muted-foreground",
  stdout: "",
  stderr: "text-status-failure",
  info: "text-muted-foreground",
  warn: "text-status-failure/80",
  error: "text-status-failure",
}

function Row({ line, top }: { line: LogLine; top: number }) {
  return (
    <div
      style={{ transform: `translateY(${top}px)`, height: ROW_HEIGHT }}
      className="hover:bg-muted/40 absolute inset-x-0 flex items-center gap-3 px-3"
    >
      <span className="text-muted-foreground/60 w-16 shrink-0 font-mono text-[10.5px] tabular-nums">
        {clockTime(line.ts)}
      </span>
      <span
        className={cn(
          "truncate font-mono text-xs whitespace-pre",
          TONE[line.kind],
        )}
        title={line.text}
      >
        {line.kind === "step" ? `▸ ${line.text}` : line.text}
      </span>
    </div>
  )
}

function clockTime(iso: string) {
  const at = new Date(iso)
  return `${String(at.getHours()).padStart(2, "0")}:${String(
    at.getMinutes(),
  ).padStart(2, "0")}:${String(at.getSeconds()).padStart(2, "0")}`
}
