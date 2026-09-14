import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// No API proxy or auth bypass. Real sessions require the same-origin HTTPS edge.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: {
    sourcemap: false,
    target: ["chrome120", "firefox128", "safari17.4"],
  },
  server: { host: "127.0.0.1", strictPort: true },
  preview: { host: "127.0.0.1", strictPort: true },
});
