import { defineConfig } from 'vite-plus'

export default defineConfig({
  pack: {
    entry: ['src/index.ts'],
    format: ['cjs'],
    platform: 'node',
    target: 'node20',
    outDir: 'lib',
    dts: false,
    clean: true,
    sourcemap: false,
    deps: {
      neverBundle: ['koishi', 'koishi-plugin-adapter-onebot'],
    },
  },
})
