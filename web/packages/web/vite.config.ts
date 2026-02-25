import { defineConfig, searchForWorkspaceRoot } from 'vite'
import vue from '@vitejs/plugin-vue'
import yaml from '@maikolib/vite-plugin-yaml'

// https://vite.dev/config/
export default defineConfig({
  plugins: [
    vue(),
    yaml(),
  ],
  server: {
    fs: {
      allow: [
        searchForWorkspaceRoot(process.cwd()),
        '/nix/store',
      ],
    },
  },
})
