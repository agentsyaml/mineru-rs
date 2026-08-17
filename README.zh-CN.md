# MinerU Rust

[English](README.md) | [简体中文](README.zh-CN.md)

使用 MinerU 将 PDF、图像和 Office 文档解析为干净的 Markdown：本项目提供 Rust 客户端库、命令行工具和本地 API 服务。PDF 渲染为纯 Rust 实现，无需 PDFium 等原生 PDF 运行时。

MinerU VLM 模型有两种运行方式：

- **远程**——让工具连接 OpenAI 兼容的 MinerU VLM 服务，无需在自己的机器上运行模型即可解析文档。
- **本地（可选）**——用 llama.cpp（`llama-server`）自行托管一个量化 MinerU 模型，并让工具连接它；见 [Docker](#docker)。

在 MinerU 3.4.5 VLM 范围内，MinerU Rust 是 MinerU Python SDK `vlm-http-client` 路径的可直接替代实现，可以完全替代该 VLM 工作流。本项目不实现、也不声称兼容非 VLM 后端。参见[兼容性契约](docs/compatibility.md)、[中文使用指南](docs/usage.md)和[英文使用指南](docs/usage.en.md)。文档大小限制控制项及其 CLI/API 适用范围见使用指南。

**没有 GPU？** MinerU Rust 只驱动 VLM 端点，纯 CPU 的替代方案是官方 MinerU Python pipeline（PP-OCRv6）：它产出相同的 `document.json` / `middle.json` / `content_list.json` / markdown 契约，两份输出可互换消费。无 VLM 服务的办公文档需要纯文本抽取时，可选用专用转换器如 [anydoc](https://github.com/firecrawl/anydoc)（仅 markdown，无版面 JSON）。官方 MinerU 4.x 同样默认走 CPU 友好路线（llama.cpp + ONNX light 栈）；`llama-server` 暴露 OpenAI 兼容端点，本项目可直接对接，但 4.x 的 `http-client` 协议未审计，不在本项目兼容契约范围内（见[兼容性说明](docs/compatibility.md)）。

远程协议固定于 MinerU `vlm-http-client` 传输基线，因此需要兼容 MinerU 的 VLM 端点——通用聊天模型无法产出版面结果。

需要 Rust 1.89 或更高版本。

## 性能

与官方 MinerU Python SDK（`vlm-http-client` 路径）实测对比，同一 VLM 端点、同一输入文档：

| 文档 | MinerU Rust | 官方 SDK | 速度 | 内存 |
| --- | --- | --- | --- | --- |
| 334 页 | 130.44 s / 2.15 GB | 162.24 s / 3.93 GB | **快 19.6%** | **省 45%** |
| 738 页 | 324.48 s / 2.31 GB | 361.74 s / 4.45 GB | **快 10.3%** | **省 48%** |

Rust 客户端全程保持更小的常驻内存：流式消费 VLM 响应并增量写出输出树，而不是把完整结果缓冲在内存中。

## 快速开始

用三个环境变量配置 VLM 服务：

| 变量 | 含义 | 示例 |
| --- | --- | --- |
| `MINERU_VL_SERVER` | VLM 服务基础 URL | `https://host/v1` |
| `MINERU_VL_MODEL_NAME` | 模型 ID | `model-id` |
| `MINERU_VL_API_KEY` | Bearer 令牌 | `your-key` |

通过 Cargo、pip 或 npm 安装 `mineru` 命令：

```sh
cargo install mineru            # Rust
pip install mineru-rs           # Python
npm install @alexsun-top/mineru # Node.js
```

然后解析文档：

```sh
mineru -p input.pdf -o out/
```

你的 markdown 会出现在 `out/` 中。可通过端点的 `GET /v1/models` 查询可用的模型 ID。成功后 `out/` 目录将包含 `document.md`、`document.json`、`middle.json`、`content_list.json`、裁剪后的 `assets/` 以及版面预览 `{stem}_layout.pdf`。

优先使用 `MINERU_VL_API_KEY` 环境变量，而非 `--api-key`，避免密钥进入 shell 历史记录。

### 本地：用 llama.cpp 托管模型

运行 compose 的 `llama-server` profile（或用你自己的量化 MinerU GGUF 模型启动 `llama-server`），然后把 `MINERU_VL_SERVER` 指向 `http://localhost:30000/v1`。见 [Docker](#docker)。

## Rust 库

```sh
cargo add mineru
```

```rust
use mineru::{RunOptions, run};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run(RunOptions::new("input.pdf", "out/")).await?;
    Ok(())
}
```

## Python

wheel 支持 CPython 3.9 及更高版本。用 `uv` 或 pip 安装：

```sh
uv add mineru-rs
# 或：pip install mineru-rs
```

`parse()` 在内存中返回 markdown 字符串，由你自己保存：

```python
import asyncio
from pathlib import Path

import mineru_rs


async def main() -> None:
    result = await mineru_rs.parse("input.pdf")
    await asyncio.to_thread(
        Path("out.md").write_text, result.markdown, encoding="utf-8"
    )


asyncio.run(main())
```

`run()` 则把完整输出树写入输出目录（见[英文使用指南](docs/usage.en.md)）。该 wheel 安装两个等效的控制台命令 `mineru` 和 `mineru-rs`；若同时安装了上游 Python `mineru` 软件包，应优先使用 `mineru-rs`，因为两者都提供 `mineru` 入口点，`PATH` 中靠前的那个会生效。发布物仅为 wheel；对于不受支持的平台或 PyPy，没有 sdist 或源代码回退方案。

Python wheel 不打包 `mineru-office-convert` 辅助程序：绑定包暂不支持 Office 格式（`.docx`/`.pptx`/`.xlsx`）输入，传入 Office 文档会报 "office conversion is unavailable"。PDF 与图像输入不受影响；需要 Office 转换时请使用 Rust CLI（`cargo install mineru --features office`）或 `mineru-api` 服务端。

## Node.js

需要 Node.js 18 或更高版本。用 `pnpm` 或 npm 安装：

```sh
pnpm add @alexsun-top/mineru
# 或：npm install @alexsun-top/mineru
```

```ts
import { writeFile } from 'node:fs/promises'
import mineru from '@alexsun-top/mineru'

const { markdown } = await mineru.parse({ path: 'input.pdf' })
await writeFile('out.md', markdown)
```

`run({ path, output })` 则把完整输出树写入 `output`（见[英文使用指南](docs/usage.en.md)）。根软件包安装两个等效二进制文件 `mineru` 和 `mineru-rs`，两者都指向 `bin/mineru.js`；若 `PATH` 上已有另一个 `mineru` 命令，应优先使用 `mineru-rs`。

npm 软件包不打包 `mineru-office-convert` 辅助程序：绑定包暂不支持 Office 格式（`.docx`/`.pptx`/`.xlsx`）输入，传入 Office 文档会报 "office conversion is unavailable"。PDF 与图像输入不受影响；需要 Office 转换时请使用 Rust CLI（`cargo install mineru --features office`）或 `mineru-api` 服务端。

## CLI 和 API 服务端

```sh
cargo install mineru
mineru --help
```

该软件包安装 `mineru`、`mineru-api` 和 `mineru-office-convert` 三个二进制文件。转换能力按 feature 可选启用：

```sh
cargo install mineru --features office          # docx/pptx/xlsx → PDF + VLM
cargo install mineru --features legacy-office   # doc/ppt/xls/odt/rtf/epub/ods/odp/csv → Markdown（无需 VLM）
cargo install mineru --features office,legacy-office
```

`mineru-api` 是 HTTP API 服务：它接收文档、调用所配置的 VLM，并返回结果归档。该服务本身不进行本地推理。

```sh
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
export MINERU_VL_API_KEY="<your-key>"

mineru-api --port 8000
```

以异步任务提交文档并获取结果：

```sh
curl -X POST http://127.0.0.1:8000/tasks \
  -F "files=@input.pdf" -F "backend=vlm-http-client" -F "response_format_zip=true"
# 轮询返回的 status_url，然后下载 result_url
```

`mineru` 客户端也可通过运行中的服务提交任务：

```sh
mineru -p input.pdf -o output --api-url http://127.0.0.1:8000
```

`--api-key` 可传入 Bearer 令牌，但应优先使用 `MINERU_VL_API_KEY`：命令行中的密钥会出现在进程列表中。

服务配置和完整选项请参见[中文使用指南](docs/usage.md)或[英文使用指南](docs/usage.en.md)。

## 安装

- **crates.io**——`cargo install mineru`（加 `--features office` 支持 docx/pptx/xlsx→PDF，加 `--features legacy-office` 支持 doc/ppt/xls/odt/rtf/epub/ods/odp/csv→Markdown；需 Rust 1.89+）。
- **Python**——`pip install mineru-rs`（支持 CPython 3.9+）。
- **Node.js**——`npm install @alexsun-top/mineru`（需 Node.js 18+）。
- **Docker**——见 [Docker](#docker)。

从源码构建：

```sh
git clone https://github.com/agentsyaml/mineru-rs
cd mineru-rs
cargo build --release
./target/release/mineru --help
```

作为库使用：`cargo add mineru`。

## 二进制文件

| 二进制 | 用途 |
| --- | --- |
| `mineru` | 主命令行工具：支持 PDF、图像和 Office 文档，可直接对接 VLM，也可通过 `mineru-api` 服务。 |
| `mineru-api` | HTTP API 服务（见上文）。 |
| `mineru-office-convert` | Office 转换辅助程序，供 `mineru` 使用：docx/pptx/xlsx→PDF（`--features office`），旧格式 doc/ppt/xls/odt/rtf/epub/ods/odp/csv→Markdown（`--features legacy-office`）。 |

## Docker

### Docker Compose 配置

仓库自带的 [`docker-compose.yaml`](docker-compose.yaml) 通过两个 profile 之一在 NVIDIA GPU 上运行 MinerU 的 OpenAI 兼容服务：

| Profile | 镜像 | 用途 |
| --- | --- | --- |
| `openai-server` | `alexsuntop/mineru:3.4.2` | vLLM 后端的 MinerU 服务（默认，端口 `30000`）。 |
| `llama-server` | `ghcr.io/ggml-org/llama.cpp:server-cuda` | llama.cpp 服务，运行量化（GGUF）MinerU 模型，端口 `30000`。 |

启动 vLLM 服务：

```sh
docker compose --profile openai-server up -d
```

启动量化的 llama.cpp 服务。将 GGUF 文件放入 `./models`（或设置 `LLAMA_MODELS_DIR`），再用 `LLAMA_MODEL` 指向容器内路径：

```sh
LLAMA_MODEL=/models/mineru-q4_k_m.gguf \
docker compose --profile llama-server up -d
# 或直接从 Hugging Face 拉取，无需本地文件：
# LLAMA_ARG_HF_REPO=your-org/mineru-gguf:Q4_K_M docker compose --profile llama-server up -d
```

两个 server profile 都暴露 `http://localhost:30000`（可用 `MINERU_PORT_OVERRIDE_VLLM` / `MINERU_PORT_OVERRIDE_LLAMA` 覆盖）；它们映射同一宿主机端口，请勿同时启动。

也可通过 `COMPOSE_PROFILES` 隐式激活 profile，例如 `COMPOSE_PROFILES=llama-server docker compose up -d`。

不再发布 CPU Docker 镜像。

## 示例

- [examples/python-uv](examples/python-uv) — 使用 `uv` 的 Python 示例
- [examples/node-pnpm](examples/node-pnpm) — 使用 `pnpm` 的 Node.js 示例

## 构建与测试

```sh
cargo build --release
cargo test
```

## 许可证

MIT OR Apache-2.0。从 Hugging Face 下载的模型权重受其模型卡所示许可证的约束。
