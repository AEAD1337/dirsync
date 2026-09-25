import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

const pkg = await import('./package.json', { with: { type: 'json' } });

// The dirsync GUI server on its default port.
const BACKEND = 'http://127.0.0.1:7373';

export default defineConfig({
  plugins: [svelte()],
  define: {
    __APP_VERSION__: JSON.stringify(pkg.default.version),
    __BUILD_TIME__: JSON.stringify(new Date().toISOString()),
    __BUILD_PROFILE__: JSON.stringify(process.env.APP_PROFILE ?? 'debug'),
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  // `npm run dev`: the backend's same-origin check accepts only its own
  // Host/Origin, so both are rewritten to the target instead of forwarding
  // localhost:5173 (which it answers with 403). The access token needs
  // nothing here: it travels in the URL hash, a header and the WS query.
  server: {
    proxy: {
      '/api': {
        target: BACKEND,
        changeOrigin: true,
        configure: (proxy) => {
          proxy.on('proxyReq', (proxyReq) => proxyReq.setHeader('origin', BACKEND));
        },
      },
      '/ws': {
        target: BACKEND,
        ws: true,
        changeOrigin: true,
        configure: (proxy) => {
          proxy.on('proxyReqWs', (proxyReq) => proxyReq.setHeader('origin', BACKEND));
        },
      },
    },
  },
});
