import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  // The sign-in page uses the logo from ../assets.
  server: { fs: { allow: [".."] } },
  test: { environment: "jsdom", include: ["src/**/*.test.{ts,tsx}"] },
});
