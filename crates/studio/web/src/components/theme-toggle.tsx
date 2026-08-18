/**
 * Dark/light toggle.
 *
 * Studio is dark by default — it sits next to a terminal — so the stylesheet's
 * bare `:root` is the dark theme and light is opted into with a `.light` class
 * on `<html>`. The matching pre-hydration script lives in `__root.tsx`; both
 * must agree on the key and the class.
 */
import { useEffect, useState } from "react"
import { Moon, Sun } from "lucide-react"

import { Button } from "@/components/ui/button"

export const THEME_KEY = "minact-studio-theme"
export const LIGHT_CLASS = "light"

function isLight() {
  return document.documentElement.classList.contains(LIGHT_CLASS)
}

export function ThemeToggle() {
  const [light, setLight] = useState(false)

  // The class is set by the inline script before React runs, and React drops
  // <html>'s attributes when it hydrates the shell — so read the live DOM on
  // mount and put the class back rather than trusting the render.
  useEffect(() => {
    const stored = localStorage.getItem(THEME_KEY)
    const prefersLight =
      stored === null
        ? window.matchMedia("(prefers-color-scheme: light)").matches
        : stored === "light"

    document.documentElement.classList.toggle(LIGHT_CLASS, prefersLight)
    setLight(prefersLight)
  }, [])

  function toggle() {
    const next = !isLight()
    document.documentElement.classList.toggle(LIGHT_CLASS, next)
    localStorage.setItem(THEME_KEY, next ? "light" : "dark")
    setLight(next)
  }

  return (
    <Button
      variant="ghost"
      size="icon"
      onClick={toggle}
      aria-label={light ? "Switch to dark theme" : "Switch to light theme"}
    >
      {light ? (
        <Moon className="size-4" aria-hidden />
      ) : (
        <Sun className="size-4" aria-hidden />
      )}
    </Button>
  )
}
