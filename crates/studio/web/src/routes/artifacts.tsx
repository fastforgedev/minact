import { useState } from "react"
import { createFileRoute } from "@tanstack/react-router"
import { useQuery } from "@tanstack/react-query"
import { ChevronRight, Download, Package } from "lucide-react"

import { artifactFileUrl, artifactQueries } from "@/lib/api"
import type { Artifact, ArtifactFile } from "@/lib/api"
import { formatRelative } from "@/components/status"
import { Button } from "@/components/ui/button"
import { Skeleton } from "@/components/ui/skeleton"
import { cn } from "@/lib/utils"

export const Route = createFileRoute("/artifacts")({
  component: ArtifactsScreen,
})

function ArtifactsScreen() {
  const artifacts = useQuery(artifactQueries.list())

  return (
    <div className="flex flex-col gap-6 p-8">
      <header className="flex flex-col gap-1">
        <h1 className="font-mono text-xl font-bold tracking-tight">
          Artifacts
        </h1>
        <p className="text-muted-foreground text-sm">
          What <code className="font-mono">actions/upload-artifact</code> left
          in <code className="font-mono">.minact-artifacts/</code>. Artifacts
          are keyed by name, so a second run with the same name replaces the
          first.
        </p>
      </header>

      {artifacts.isPending ? (
        <div className="flex flex-col gap-2">
          {[0, 1].map((index) => (
            <Skeleton key={index} className="h-20 w-full rounded-lg" />
          ))}
        </div>
      ) : null}

      {artifacts.data?.length === 0 ? (
        <div className="bg-card flex items-start gap-3 rounded-lg border p-5">
          <Package
            className="text-muted-foreground mt-0.5 size-4 shrink-0"
            aria-hidden
          />
          <div className="flex flex-col gap-1">
            <p className="text-sm font-medium">No artifacts yet</p>
            <p className="text-muted-foreground text-sm">
              A step that uses <code>actions/upload-artifact</code> puts its
              files here.
            </p>
          </div>
        </div>
      ) : null}

      <ul className="flex flex-col gap-2">
        {artifacts.data?.map((artifact) => (
          <li key={artifact.name}>
            <ArtifactCard artifact={artifact} />
          </li>
        ))}
      </ul>
    </div>
  )
}

function ArtifactCard({ artifact }: { artifact: Artifact }) {
  const [open, setOpen] = useState(false)

  return (
    <div className="bg-card rounded-lg border">
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        aria-expanded={open}
        className="hover:bg-accent/40 focus-visible:outline-ring flex w-full items-center gap-3 rounded-lg p-4 text-left transition-colors focus-visible:outline-2 focus-visible:-outline-offset-2"
      >
        <ChevronRight
          className={cn(
            "text-muted-foreground size-4 shrink-0 transition-transform",
            open && "rotate-90",
          )}
          aria-hidden
        />
        <span className="min-w-0 flex-1 truncate font-mono text-sm font-medium">
          {artifact.name}
        </span>
        <span className="text-muted-foreground font-mono text-xs tabular-nums">
          {artifact.file_count}{" "}
          {artifact.file_count === 1 ? "file" : "files"} ·{" "}
          {formatBytes(artifact.total_bytes)}
        </span>
        {artifact.modified ? (
          <span className="text-muted-foreground w-20 text-right font-mono text-xs">
            {formatRelative(artifact.modified)}
          </span>
        ) : null}
      </button>

      {open ? (
        <ul className="border-t">
          {artifact.files.map((file) => (
            <li key={file.path}>
              <FileRow artifactName={artifact.name} file={file} />
            </li>
          ))}
          {artifact.files.length === 0 ? (
            <p className="text-muted-foreground p-4 font-mono text-xs">
              This artifact is empty — the uploaded path did not exist.
            </p>
          ) : null}
        </ul>
      ) : null}
    </div>
  )
}

function FileRow({
  artifactName,
  file,
}: {
  artifactName: string
  file: ArtifactFile
}) {
  const [previewing, setPreviewing] = useState(false)
  const url = artifactFileUrl(artifactName, file.path)

  const preview = useQuery({
    queryKey: ["artifact-preview", artifactName, file.path],
    queryFn: async () => {
      const response = await fetch(url)
      if (!response.ok) throw new Error(`Could not read ${file.path}`)
      return response.text()
    },
    enabled: previewing,
  })

  return (
    <div className="flex flex-col">
      <div className="hover:bg-accent/30 flex items-center gap-3 px-4 py-2 transition-colors">
        <span className="min-w-0 flex-1 truncate font-mono text-xs">
          {file.path}
        </span>
        <span className="text-muted-foreground font-mono text-[11px] tabular-nums">
          {formatBytes(file.bytes)}
        </span>

        {file.previewable ? (
          <Button
            variant="ghost"
            size="sm"
            className="h-7 text-xs"
            aria-expanded={previewing}
            onClick={() => setPreviewing((value) => !value)}
          >
            {previewing ? "Hide" : "View"}
          </Button>
        ) : null}

        {/* A plain link: the server sets the type, the browser does the rest. */}
        <a
          href={url}
          download
          className="text-muted-foreground hover:text-foreground focus-visible:outline-ring rounded p-1 transition-colors focus-visible:outline-2"
          aria-label={`Download ${file.path}`}
        >
          <Download className="size-3.5" aria-hidden />
        </a>
      </div>

      {previewing ? (
        <div className="border-t">
          {preview.isPending ? (
            <p className="text-muted-foreground p-4 font-mono text-xs">
              Reading…
            </p>
          ) : preview.isError ? (
            <p className="text-status-failure p-4 font-mono text-xs">
              {preview.error.message}
            </p>
          ) : (
            <pre className="bg-muted/30 max-h-96 overflow-auto p-4 font-mono text-xs leading-relaxed">
              {preview.data}
            </pre>
          )}
        </div>
      ) : null}
    </div>
  )
}

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}
