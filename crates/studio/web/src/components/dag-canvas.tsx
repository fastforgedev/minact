/**
 * The job DAG.
 *
 * Hand-drawn SVG rather than a graph library: the scheduler already returns
 * the jobs grouped into execution layers, so layout is one column per layer
 * and there is nothing left for a layout engine to solve.
 *
 * The same canvas renders a workflow's plan and a run in progress — a run just
 * supplies conclusions, which colour the nodes.
 */
import { useMemo } from "react"
import type { Conclusion } from "@/lib/api"
import { cn } from "@/lib/utils"

const NODE_W = 184
const NODE_H = 56
const GAP_X = 88
const GAP_Y = 16
const PAD = 12
/** Room under the columns for the layer captions. */
const CAPTION_H = 22

export type DagNode = {
  id: string
  name: string
  /** Small second line — step count, duration, whatever fits. */
  detail?: string
  /** `undefined` for a plan, `null` for a job that has not concluded. */
  conclusion?: Conclusion | null
  running?: boolean
}

type Placed = DagNode & { x: number; y: number }

type Props = {
  layers: Array<Array<string>>
  edges: Array<{ from: string; to: string }>
  nodes: Array<DagNode>
  selectedId?: string
  onSelect?: (id: string) => void
  className?: string
}

export function DagCanvas({
  layers,
  edges,
  nodes,
  selectedId,
  onSelect,
  className,
}: Props) {
  const byId = useMemo(
    () => new Map(nodes.map((node) => [node.id, node])),
    [nodes],
  )

  // A cyclic graph has no layers, but the edges are still worth showing —
  // seeing the loop is the whole point. Fall back to a single column.
  const columns = layers.length > 0 ? layers : [nodes.map((node) => node.id)]

  const placed = useMemo(() => {
    const tallest = Math.max(...columns.map((column) => column.length), 1)
    const columnHeight = tallest * NODE_H + (tallest - 1) * GAP_Y

    const result: Array<Placed> = []
    columns.forEach((column, columnIndex) => {
      const height = column.length * NODE_H + (column.length - 1) * GAP_Y
      const top = PAD + (columnHeight - height) / 2

      column.forEach((id, rowIndex) => {
        const node = byId.get(id)
        if (!node) return
        result.push({
          ...node,
          x: PAD + columnIndex * (NODE_W + GAP_X),
          y: top + rowIndex * (NODE_H + GAP_Y),
        })
      })
    })
    return result
  }, [columns, byId])

  const positions = useMemo(
    () => new Map(placed.map((node) => [node.id, node])),
    [placed],
  )

  const width = PAD * 2 + columns.length * NODE_W + (columns.length - 1) * GAP_X
  const height =
    PAD * 2 +
    CAPTION_H +
    Math.max(...columns.map((column) => column.length), 1) * (NODE_H + GAP_Y) -
    GAP_Y

  if (placed.length === 0) {
    return (
      <p className="text-muted-foreground p-6 text-sm">
        Nothing to graph yet.
      </p>
    )
  }

  return (
    <div className={cn("overflow-x-auto", className)}>
      <svg
        viewBox={`0 0 ${width} ${height}`}
        width={width}
        height={height}
        role="img"
        aria-label={columns
          .map((column, index) => `Layer ${index + 1}: ${column.join(", ")}`)
          .join(". ")}
        className="max-w-full"
      >
        <defs>
          <marker
            id="dag-arrow"
            viewBox="0 0 8 8"
            refX="7"
            refY="4"
            markerWidth="6"
            markerHeight="6"
            orient="auto"
          >
            <path d="M0 0 L8 4 L0 8 z" className="fill-border" />
          </marker>
        </defs>

        <g className="stroke-border" fill="none" strokeWidth={1.4}>
          {edges.map((edge) => {
            const from = positions.get(edge.from)
            const to = positions.get(edge.to)
            if (!from || !to) return null

            const x1 = from.x + NODE_W
            const y1 = from.y + NODE_H / 2
            const x2 = to.x
            const y2 = to.y + NODE_H / 2
            const bend = Math.max(GAP_X / 2, 24)

            return (
              <path
                key={`${edge.from}->${edge.to}`}
                d={`M${x1} ${y1} C ${x1 + bend} ${y1}, ${x2 - bend} ${y2}, ${x2} ${y2}`}
                markerEnd="url(#dag-arrow)"
              />
            )
          })}
        </g>

        {placed.map((node) => (
          <JobNode
            key={node.id}
            node={node}
            selected={node.id === selectedId}
            onSelect={onSelect}
          />
        ))}

        {columns.map((_, index) => (
          <text
            key={index}
            x={PAD + index * (NODE_W + GAP_X)}
            y={height - 6}
            className="fill-muted-foreground font-mono text-[10px] tracking-[0.14em] uppercase"
          >
            {layers.length > 0 ? `Layer ${index + 1}` : "Unresolved"}
          </text>
        ))}
      </svg>
    </div>
  )
}

/** Border colour follows the conclusion; unfinished jobs stay neutral. */
function nodeTone(node: DagNode) {
  if (node.running) return "stroke-status-running"
  switch (node.conclusion) {
    case "success":
      return "stroke-status-success"
    case "failure":
      return "stroke-status-failure"
    case "cancelled":
      return "stroke-status-failure/60"
    case "skipped":
      return "stroke-status-skipped/60"
    default:
      return "stroke-border"
  }
}

function dotTone(node: DagNode) {
  if (node.running) return "fill-status-running"
  switch (node.conclusion) {
    case "success":
      return "fill-status-success"
    case "failure":
      return "fill-status-failure"
    case "cancelled":
      return "fill-status-failure/60"
    case "skipped":
      return "fill-status-skipped/60"
    default:
      return "fill-muted-foreground/40"
  }
}

function JobNode({
  node,
  selected,
  onSelect,
}: {
  node: Placed
  selected: boolean
  onSelect?: (id: string) => void
}) {
  const interactive = Boolean(onSelect)
  const showsStatus = node.conclusion !== undefined || node.running

  return (
    <g
      transform={`translate(${node.x} ${node.y})`}
      role={interactive ? "button" : undefined}
      tabIndex={interactive ? 0 : undefined}
      aria-pressed={interactive ? selected : undefined}
      onClick={() => onSelect?.(node.id)}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault()
          onSelect?.(node.id)
        }
      }}
      className={cn(
        "focus-visible:outline-ring focus-visible:outline-2 focus-visible:outline-offset-2",
        interactive && "cursor-pointer",
      )}
    >
      <rect
        width={NODE_W}
        height={NODE_H}
        rx={6}
        className={cn(
          "fill-card transition-colors",
          nodeTone(node),
          selected && "stroke-primary",
          interactive && !selected && "hover:stroke-muted-foreground",
        )}
        strokeWidth={selected ? 1.8 : 1.2}
      />

      {showsStatus ? (
        <circle
          cx={16}
          cy={NODE_H / 2}
          r={4}
          className={cn(
            dotTone(node),
            node.running && "motion-safe:animate-pulse",
          )}
        />
      ) : null}

      <text
        x={showsStatus ? 30 : 14}
        y={23}
        className="fill-foreground font-mono text-[12.5px] font-semibold"
      >
        {truncate(node.name, showsStatus ? 19 : 21)}
      </text>
      {node.detail ? (
        <text
          x={showsStatus ? 30 : 14}
          y={40}
          className="fill-muted-foreground text-[11px]"
        >
          {truncate(node.detail, showsStatus ? 22 : 24)}
        </text>
      ) : null}
    </g>
  )
}

function truncate(value: string, max: number) {
  return value.length > max ? `${value.slice(0, max - 1)}…` : value
}
