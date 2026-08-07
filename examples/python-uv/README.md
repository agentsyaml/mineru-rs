# mineru-example (python)

1. `uv sync` — creates a venv and installs `mineru-rs` from pyproject.toml (generates a lockfile on first run).
2. Set the env vars: `export MINERU_VL_SERVER=https://host/v1 MINERU_VL_MODEL_NAME=...` (and `MINERU_VL_API_KEY=...` if required).
3. `uv run python main.py input.pdf` — markdown is written to `output/document.md`.

`mineru_rs.parse(path)` returns a `ParseResult` (with `markdown` and `warnings` fields); `mineru_rs.run(path, "output")` writes the full output tree instead.
