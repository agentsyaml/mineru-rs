// Minimal @alexsun-top/mineru example: parse one file, save the returned markdown.
//
// Env vars for the MinerU service:
//   MINERU_VL_SERVER      e.g. https://host/v1
//   MINERU_VL_MODEL_NAME  the model name to use
//   MINERU_VL_API_KEY     optional API key

import fs from 'node:fs/promises'
import path from 'node:path'
import mineru from '@alexsun-top/mineru'

const input = process.argv[2] ?? 'input.pdf'

const { markdown, warnings } = await mineru.parse({ path: input })

const out = path.join('output', 'document.md')
await fs.mkdir(path.dirname(out), { recursive: true })
await fs.writeFile(out, markdown, 'utf-8')

console.log(`parsed ${input}: markdown ${markdown.length} chars -> ${out}`)
if (warnings?.length) {
  console.log(`warnings (${warnings.length}):`)
  for (const w of warnings) console.log(`  - ${w}`)
}
