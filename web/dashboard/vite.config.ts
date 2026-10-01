// The dashboard is served by octopage-server from its own origin. In development, Vite
// serves it and passes the API through to a service at OCTOPAGE_URL (default
// http://localhost:8080), presenting the service's own origin so session changes pass its
// same-origin check.
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

const service = process.env.OCTOPAGE_URL ?? 'http://localhost:8080';
const passThrough = {
  target: service,
  changeOrigin: true,
  configure: (proxy: any) => {
    proxy.on('proxyReq', (request: any) => {
      if (request.getHeader('origin')) request.setHeader('origin', service);
    });
  },
};

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: { '/v1': passThrough, '/auth': passThrough, '/healthz': passThrough },
  },
  build: { sourcemap: true, chunkSizeWarningLimit: 1024 },
});
