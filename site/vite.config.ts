import { defineConfig } from 'vite'

export default defineConfig({
  base: '/fancontrol/',
  build: {
    outDir: '../docs',
    emptyOutDir: true,
  },
})
