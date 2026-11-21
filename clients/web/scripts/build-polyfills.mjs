import {createRequire} from 'node:module';
import {dirname,join,resolve} from 'node:path';
import {mkdir} from 'node:fs/promises';
import {fileURLToPath} from 'node:url';

const root=fileURLToPath(new URL('../',import.meta.url));
export async function buildPolyfills(output) {
  const sourceRequire = createRequire(new URL('../apps/source-runner/package.json', import.meta.url));
  const { build } = sourceRequire('esbuild');
  await mkdir(dirname(output), { recursive: true });
  // URL, URLSearchParams, and DOMParser come from polyfills; TextDecoder/TextEncoder/atob/btoa/crypto
  // are backed by host ICU & Buffer bridges inside source-runner for full multi-charset support (GBK, Shift-JIS, EUC-JP, Big5, UTF-8).
  await build({
    stdin: {
      contents:
        "import URL from 'core-js-pure/actual/url/index.js'; import URLSearchParams from 'core-js-pure/actual/url-search-params/index.js'; import {DOMParser} from 'linkedom/worker'; Object.assign(globalThis,{URL,URLSearchParams,DOMParser});",
      resolveDir: join(root, 'apps/source-runner'),
      sourcefile: 'runtime-polyfills.js',
    },
    bundle: true,
    platform: 'browser',
    format: 'iife',
    outfile: output,
    logLevel: 'warning',
  });
}
if(process.argv[1] && resolve(process.argv[1])===fileURLToPath(import.meta.url)) await buildPolyfills(resolve(process.argv[2]));
