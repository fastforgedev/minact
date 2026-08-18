import {
  HeadContent,
  Scripts,
  createRootRoute,
  useRouter,
} from "@tanstack/react-router"
import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { TanStackRouterDevtoolsPanel } from "@tanstack/react-router-devtools"
import { TanStackDevtools } from "@tanstack/react-devtools"
import { useState } from "react"

import appCss from "../styles.css?url"
import { AppShell } from "@/components/app-shell"
import { LIGHT_CLASS, THEME_KEY } from "@/components/theme-toggle"
import { Button } from "@/components/ui/button"
import { TooltipProvider } from "@/components/ui/tooltip"

/**
 * Applied before first paint so a light-theme user never sees a dark flash.
 * Dark needs no class — it is the stylesheet's default — so this only ever
 * adds `.light`. `ThemeToggle` re-applies it after hydration, because React
 * discards `<html>`'s attributes when it takes over the document.
 */
const THEME_SCRIPT = `
(function () {
  try {
    var stored = localStorage.getItem(${JSON.stringify(THEME_KEY)});
    var light = stored
      ? stored === "light"
      : window.matchMedia("(prefers-color-scheme: light)").matches;
    document.documentElement.classList.toggle(${JSON.stringify(LIGHT_CLASS)}, light);
  } catch (e) {
    /* Dark is the default; leaving the class off is the correct fallback. */
  }
})();
`

export const Route = createRootRoute({
  head: () => ({
    meta: [
      { charSet: "utf-8" },
      { name: "viewport", content: "width=device-width, initial-scale=1" },
      { title: "minact studio" },
    ],
    links: [{ rel: "stylesheet", href: appCss }],
  }),
  notFoundComponent: NotFound,
  errorComponent: ErrorScreen,
  shellComponent: RootDocument,
})

function RootDocument({ children }: { children: React.ReactNode }) {
  // One client per document, created lazily so it is never shared across
  // renders in dev.
  const [queryClient] = useState(
    () =>
      new QueryClient({
        defaultOptions: {
          queries: { staleTime: 5_000, refetchOnWindowFocus: false },
        },
      }),
  )

  return (
    <html lang="en">
      <head>
        <HeadContent />
        <script dangerouslySetInnerHTML={{ __html: THEME_SCRIPT }} />
      </head>
      <body>
        <QueryClientProvider client={queryClient}>
          <TooltipProvider delayDuration={200}>
            <AppShell>{children}</AppShell>
          </TooltipProvider>
        </QueryClientProvider>
        <TanStackDevtools
          config={{ position: "bottom-right" }}
          plugins={[
            {
              name: "Tanstack Router",
              render: <TanStackRouterDevtoolsPanel />,
            },
          ]}
        />
        <Scripts />
      </body>
    </html>
  )
}

function NotFound() {
  return (
    <div className="flex flex-col gap-2 p-10">
      <h1 className="font-mono text-lg font-bold">Nothing here</h1>
      <p className="text-muted-foreground text-sm">
        That URL does not match any Studio screen.
      </p>
    </div>
  )
}

function ErrorScreen({ error }: { error: Error }) {
  const router = useRouter()

  return (
    <div className="flex flex-col items-start gap-3 p-10">
      <h1 className="font-mono text-lg font-bold">Studio hit an error</h1>
      <p className="text-muted-foreground max-w-prose text-sm">
        {error.message}
      </p>
      <Button variant="outline" size="sm" onClick={() => router.invalidate()}>
        Try again
      </Button>
    </div>
  )
}
