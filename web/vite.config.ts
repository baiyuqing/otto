import path from "node:path";
import { fileURLToPath } from "node:url";

import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const currentDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(currentDir, "..");

export default defineConfig({
  plugins: [react()],
  server: {
    fs: {
      allow: [repoRoot],
    },
    proxy: {
      "/api": process.env.OTTO_COLLAB_API_TARGET ?? "http://127.0.0.1:4318",
    },
  },
  resolve: {
    alias: {
      "@repo": repoRoot,
    },
  },
});
