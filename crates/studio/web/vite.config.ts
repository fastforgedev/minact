import { defineConfig } from "vite"
import { devtools } from "@tanstack/devtools-vite"
import { tanstackStart } from "@tanstack/react-start/plugin/vite"
import viteReact from "@vitejs/plugin-react"
import tailwindcss from "@tailwindcss/vite"

/**
 * Studio ships as a SPA embedded in the `minact` binary — the backend is the
 * Rust engine, so there is no Node runtime at serve time. SSR and server
 * functions are off; `spa.prerender` emits a static shell that the Rust server
 * returns for every non-`/api` path.
 */
const config = defineConfig({
  resolve: { tsconfigPaths: true },
  plugins: [
    devtools(),
    tailwindcss(),
    tanstackStart({
      spa: {
        enabled: true,
        prerender: { outputPath: "/index.html" },
      },
    }),
    viteReact(),
  ],
  server: {
    port: 3000,
    proxy: {
      "/api": {
        target: "http://127.0.0.1:4000",
        changeOrigin: true,
      },
    },
  },
})

export default config
