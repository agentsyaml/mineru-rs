# Node.js example (pnpm)

Requires Node.js 18+ and a MinerU-compatible VLM endpoint. This example's
`package.json` requires `@alexsun-top/mineru` `^0.3.1`. Check that exact
version is available before installing the project dependency:

```sh
npm view @alexsun-top/mineru@0.3.1 version
```

The checkout version does not establish registry or release availability. For
a CLI-only run, resolve and invoke the latest version currently on npm:

```bash
export MINERU_VL_SERVER="https://host/v1"
export MINERU_VL_MODEL_NAME="model-id"
# If required, enter the key without echoing it or saving it in shell history:
read -rs -p "VLM API key: " MINERU_VL_API_KEY; export MINERU_VL_API_KEY
printf '\n'
version="$(npm view @alexsun-top/mineru version)"
npx --yes --package="@alexsun-top/mineru@$version" -- mineru-rs -p input.pdf -o output
```

For the TypeScript example below, the declared dependency must first be
available from npm.

When the example dependency is available, run from this directory:

```bash
pnpm install
export MINERU_VL_SERVER="https://host/v1"
export MINERU_VL_MODEL_NAME="model-id"
# If required, enter the key without echoing it or saving it in shell history:
read -rs -p "VLM API key: " MINERU_VL_API_KEY; export MINERU_VL_API_KEY
printf '\n'
pnpm start input.pdf
```

The script writes Markdown to `output/document.md` and prints any warnings.
`mineru.parse({ path })` returns `{ markdown, warnings }`;
`mineru.run({ path, output })` writes the full result tree.
