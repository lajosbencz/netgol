import { defineConfig } from 'vite';

// In dev, vite serves the SPA on :5173 and proxies WebSocket upgrades to the
// Rust server on :8080. In prod, nginx fronts both - see web/nginx.conf.
export default defineConfig({
  server: {
    host: '0.0.0.0',
    port: 5173,
    proxy: {
      '/ws': {
        target: process.env.VITE_PROXY_TARGET ?? 'http://localhost:8080',
        ws: true,
        changeOrigin: false,
      },
    },
  },
});
