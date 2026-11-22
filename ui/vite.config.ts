import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

// `npm run dev` serves the UI with hot reload and forwards API calls to a
// running `swe serve`. Start the server with
// `--dev-origin http://localhost:5173` so it accepts this origin.
const backend = `http://127.0.0.1:${process.env.SWE_AGENT_PORT ?? '4477'}`;

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: { '/api': { target: backend, changeOrigin: false } },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    chunkSizeWarningLimit: 1200,
  },
});
