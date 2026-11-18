// The page is built into ../assets, which the server embeds in its binary:
// one script, one style sheet and the page, under names that never change,
// with nothing inline so that the server's content security policy holds.
import { defineConfig, type Plugin } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { join, sep } from 'node:path';

// What the built page was built from, as a hash of every source file, so
// that a test of the server can tell when the page was not rebuilt after
// a change (crates/factory-dash/tests/serve.rs computes the same).
const SOURCES = ['index.html', 'package.json', 'package-lock.json', 'svelte.config.js', 'vite.config.ts', 'tsconfig.json', 'src'];

function files(root: string, at: string): string[] {
  const path = join(root, at);
  if (!statSync(path).isDirectory()) return [at];
  return readdirSync(path).flatMap((name) => files(root, join(at, name)));
}

export function sourceHash(root: string): string {
  let hash = 0xcbf29ce484222325n;
  const eat = (bytes: Uint8Array) => {
    for (const byte of bytes) hash = ((hash ^ BigInt(byte)) * 0x100000001b3n) & 0xffffffffffffffffn;
  };
  const all = SOURCES.flatMap((at) => files(root, at)).map((at) => at.split(sep).join('/')).sort();
  for (const name of all) {
    eat(new TextEncoder().encode(name));
    eat(new Uint8Array([0]));
    eat(readFileSync(join(root, name)));
    eat(new Uint8Array([0]));
  }
  return hash.toString(16).padStart(16, '0');
}

const stamp = (): Plugin => ({
  name: 'source-stamp',
  closeBundle() {
    writeFileSync(join(__dirname, '../assets/source.txt'), `${sourceHash(__dirname)}\n`);
  },
});

export default defineConfig({
  plugins: [svelte(), stamp()],
  build: {
    outDir: '../assets',
    emptyOutDir: true,
    modulePreload: false,
    cssCodeSplit: false,
    assetsInlineLimit: 0,
    target: 'es2022',
    rollupOptions: {
      output: {
        entryFileNames: 'app.js',
        assetFileNames: 'app.[ext]',
        inlineDynamicImports: true,
      },
    },
  },
  server: {
    // `npm run dev` beside a dashboard serving on its default port.
    proxy: { '/api': 'http://127.0.0.1:8717' },
  },
});
