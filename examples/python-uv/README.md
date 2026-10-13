# Python example (uv)

Requires CPython 3.9+ and a MinerU-compatible VLM endpoint. From this directory:

```bash
uv sync
export MINERU_VL_SERVER="https://host/v1"
export MINERU_VL_MODEL_NAME="model-id"
# If required, enter the key without echoing it or saving it in shell history:
read -rs -p "VLM API key: " MINERU_VL_API_KEY; export MINERU_VL_API_KEY
printf '\n'
uv run python main.py input.pdf
```

The script writes Markdown to `output/document.md` and prints any warnings.
`mineru_rs.parse(path)` returns a `ParseResult` with `markdown` and `warnings`;
`mineru_rs.run(path, "output")` writes the full result tree. Check the release
guide for current package availability before pinning a version; the checkout
version alone does not establish publication.
