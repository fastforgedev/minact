import { createFileRoute, redirect } from "@tanstack/react-router"

// Studio has no dashboard of its own — the workflow list is the front door.
export const Route = createFileRoute("/")({
  beforeLoad: () => {
    throw redirect({ to: "/workflows" })
  },
})
