# mineru-example (node)

1. `pnpm install` — installs `@alexsun-top/mineru`, `typescript`, and `tsx`.
2. Set the env vars: `export MINERU_VL_SERVER=https://host/v1 MINERU_VL_MODEL_NAME=...` (and `MINERU_VL_API_KEY=...` if required).
3. `pnpm start input.pdf` (or `pnpm exec tsx main.ts input.pdf`) — markdown is written to `output/document.md`.

`mineru.parse({ path })` resolves to `{ markdown, warnings }`; `mineru.run({ path, output })` writes the full output tree instead.
