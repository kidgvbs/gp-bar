import { defineConfig } from "vite";

// Tauri expects a fixed port; fail instead of picking a random one.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "safari15",
    minify: "esbuild",
    sourcemap: false,
  },
});
