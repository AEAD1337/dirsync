import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

const pkg = await import('./package.json', { with: { type: 'json' } });

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
  server: {
    proxy: {
      '/api': 'http://localhost:7373',
      '/ws': { target: 'ws://localhost:7373', ws: true },
    },
  },
});
