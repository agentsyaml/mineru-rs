// Minimal @alexsun-top/mineru example: parse one file, save the returned markdown.

// Env vars for the MinerU service:
//   MINERU_VL_SERVER      e.g. https://host/v1
//   MINERU_VL_MODEL_NAME  the model name to use
//   MINERU_VL_API_KEY     optional API key

import fs from "node:fs";
import path from "node:path";
import mineru from "@alexsun-top/mineru";

const input = process.argv[2] ?? "input.pdf";

if (!fs.existsSync(input)) {
  console.error(`error: '${input}' not found`);
  console.error("usage: pnpm start input.pdf");
  process.exit(1);
}

const result = await mineru.parse({ path: input });

const out = path.join("output", "document.md");
fs.mkdirSync(path.dirname(out), { recursive: true });
fs.writeFileSync(out, result.markdown, "utf-8");

console.log(`parsed ${input}: markdown ${result.markdown.length} chars -> ${out}`);
if (result.warnings?.length) {
  console.log(`warnings (${result.warnings.length}):`);
  for (const w of result.warnings) console.log(`  - ${w}`);
}
