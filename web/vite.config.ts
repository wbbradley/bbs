import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// `npm run dev` proxies the API to a local `bbs serve --web 127.0.0.1:7778`.
export default defineConfig({
  plugins: [react()],
  server: {
    // The sign-in page uses the logo from ../assets.
    fs: { allow: [".."] },
    proxy: { "/api": { target: process.env.BBS_WEB ?? "http://127.0.0.1:7778" } },
  },
  build: { outDir: "dist", emptyOutDir: true, chunkSizeWarningLimit: 1024 },
});
