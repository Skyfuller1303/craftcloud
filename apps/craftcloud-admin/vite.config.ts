import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// The app is served by craftcloud-server under /admin on the same origin as
// the API (ADR-0015), so asset URLs are rooted at /admin/ and no CORS is needed.
export default defineConfig({
  base: "/admin/",
  plugins: [react()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
    // The CSP forbids inline scripts and styles; never inline assets as data URLs.
    assetsInlineLimit: 0,
  },
  server: {
    // Development only: forward API calls to a locally running craftcloud-server.
    proxy: { "/api": "http://127.0.0.1:8080" },
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    include: ["src/**/*.test.{ts,tsx}", "scripts/**/*.test.ts"],
    css: false,
  },
});
