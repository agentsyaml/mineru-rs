# MinerU Rust

[English](README.md) | [简体中文](README.zh-CN.md)

使用 MinerU 将 PDF、图像和 Office 文档解析为干净的 Markdown：本项目提供 Rust 客户端库、命令行工具和本地 API 服务。PDF 渲染为纯 Rust 实现，无需 PDFium 等原生 PDF 运行时。

MinerU VLM 模型有两种运行方式：

- **远程**——让工具连接 OpenAI 兼容的 MinerU VLM 服务，无需在自己的机器上运行模型即可解析文档。
- **外部服务（可选）**——让工具连接一个在仓库外准备好的 OpenAI 兼容服务，例如 `llama-server`；见 [Docker](#docker)。这是未经验证的 provider 示例，不表示兼容 MinerU 4.0.0a6 的多模态协议或模型。这与 CLI 的 `backend=local` 不同：后者通过隔离的 Rust `mineru-office-convert` 辅助程序调用 AnyDoc，抽取支持的旧格式和干净文本 PDF Markdown；该辅助程序不启动 Python、Microsoft Office/LibreOffice，不加载模型，也不发网络请求。

在 MinerU 3.4.5 VLM 范围内，MinerU Rust 是 MinerU Python SDK `vlm-http-client` 路径的可直接替代实现，可以完全替代该 VLM 工作流。直接 `backend=hybrid-http-client` 是独立的官方 MinerU 4.0.0a6 边界：要求用户安装精确版本的 Python 包；未指定 worker 模式时，一个可运行文档使用一个带内嵌 shim 的子进程，多个可运行文档使用 persistent worker。显式 `--official-worker-mode` 或 `MINERU_OFFICIAL_WORKER_MODE` 会覆盖该自动选择。输出写入独立的 `hybrid-v4` 路径。CLI 另提供独立的 `backend=local` AnyDoc 原生 Markdown 路径；这是项目私有的 native lane，不是官方 Hybrid。API Hybrid 仍 fail-closed，不会冒充旧 3.4.5 VLM 路径。参见[兼容性契约](docs/compatibility.md)、[中文使用指南](docs/usage.md)和[英文使用指南](docs/usage.en.md)。文档大小限制控制项及其 CLI/API 适用范围见使用指南。

**没有 GPU？** PDF/图像流水线仍驱动 VLM 端点；需要完整版面解析时，纯 CPU 的替代方案是官方 MinerU Python pipeline（PP-OCRv6）：它产出相同的 `document.json` / `middle.json` / `content_list.json` / markdown 契约，两份输出可互换消费。非 local CLI 路径的旧格式会先由隔离 helper 尽力生成有界的仅文本 PDF，再进入现有 PDF/VLM 路径；原版式、图片、表格、公式和宏可能丢失，非 ASCII 字符可能变成 `?`，若不适用请先用 Microsoft Office 或 LibreOffice 保存为 DOCX/XLSX/PPTX。无 VLM 服务时仍可用 `backend=local`（通过隔离 Rust helper 运行 AnyDoc，仅 Markdown、无版面 JSON）；该路径不启动 Python、Office，不加载模型，也不发网络请求。不确定的 PDF 会明确失败，不伪造 official 输出。官方 MinerU 4.0.0a6 直接 Hybrid 支持 `medium`、`high`、`xhigh` 和 `auto|light|full` 模型栈；Python、MinerU 与模型文件不随 Rust 二进制打包。Docker/llama.cpp 编排不属于此边界（见[兼容性说明](docs/compatibility.md)）。

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

### 可选的外部 OpenAI 兼容服务

只有在仓库外明确准备好模型、配置并完成挂载后，才运行 compose 的
`llama-server` profile（或自行启动 `llama-server`），然后把
`MINERU_VL_SERVER` 指向 `http://localhost:30000/v1`。本项目不验证该 provider
或模型兼容性，也不表示会自动从 Hugging Face 下载模型。见 [Docker](#docker)。

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
cargo install mineru --features legacy-office   # 旧格式 → 尽力文本 PDF + VLM；local/native PDF → Markdown
cargo install mineru --features office,legacy-office
```

`mineru --backend local` 支持上述 `legacy-office` 旧格式和干净文本 PDF，并通过隔离且有界的 Rust `mineru-office-convert` 辅助程序运行 AnyDoc：不启动 Python、Microsoft Office/LibreOffice，不调用模型，也不发网络请求。旧格式输出为 `output/{stem}/office/{stem}.md`，native PDF 输出为 `output/{stem}/native/{stem}.md`，后者只有 Markdown 文件，不生成 `document.json`、`middle.json`、`content-list` 或 assets。扫描、混合、乱码、低质量或不确定的 PDF 会明确失败，不回退到 VLM；`backend=local` 也不是本地 `llama-server` 后端。local 使用辅助程序的有界默认策略；helper 专属的 wall/CPU/内存/NOFILE/进程隔离参数在未明确支持时会明确拒绝，不会静默忽略。当前仅支持输入/输出字节限制配置。`mineru-api` 的 backend 语义保持不变。

`--backend hybrid-http-client` 在直接模式使用官方 MinerU 4.0.0a6 worker；API 模式仍明确拒绝，不会静默进入旧 VLM 路径。默认 `vlm-http-client` 始终保持现有远程 VLM 行为。

不使用 `backend=local` 时，同一组旧格式会由隔离 helper 先生成有界的仅文本尽力 PDF，再进入现有 VLM 路径。该转换可能丢失 Office 原版式、图片、表格、公式或宏，非 ASCII 字符可能变为 `?`；若这些细节重要，请先用 Microsoft Office 或 LibreOffice 转存为 DOCX/XLSX/PPTX。

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

- **crates.io**——`cargo install mineru`（加 `--features office` 支持 docx/pptx/xlsx→PDF，加 `--features legacy-office` 支持非 local CLI 的旧格式尽力文本 PDF 转换；需 Rust 1.89+）。
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
| `mineru-office-convert` | 内置 Rust 辅助程序：docx/pptx/xlsx→PDF（`--features office`），旧格式 doc/ppt/xls/odt/rtf/epub/ods/odp/csv→非 local VLM 使用的有界尽力文本 PDF，以及 `backend=local` 使用的隔离旧格式/native PDF Markdown（`--features legacy-office`）。local 路径不启动 Python、Office，不加载模型，也不发网络请求。 |

## Docker

### 已发布的 Rust API 镜像

已发布的 CPU Rust API 镜像为 `ghcr.io/agentsyaml/mineru-cli`。它监听容器
端口 `8000`，提供 `GET /health`，将任务输出写入 `/app/output`，并以默认的
非 root 用户运行。已发布版本的 Rust 二进制使用 `office,legacy-office`
feature；这不包含 Python 或本地模型推理。

该镜像只包含 Rust 二进制：不包含 Python、`mineru==4.0.0a6` 或模型文件。
因此，官方 Hybrid 需要显式提供一个另行准备好的环境；API Hybrid 在该镜像中仍
fail-closed，绝不会冒充 3.4.5 VLM 路径。

```sh
mkdir -p output
chmod a+rwx output  # 让镜像的非 root 用户可以写入
docker run --rm \
  --publish 127.0.0.1:8000:8000 \
  --volume "$PWD/output:/app/output" \
  --env MINERU_VL_SERVER="https://<server>" \
  --env MINERU_VL_MODEL_NAME="<model-id>" \
  --env MINERU_VL_API_KEY="<your-key>" \
  --env MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT=true \
  ghcr.io/agentsyaml/mineru-cli:latest

curl http://127.0.0.1:8000/health
```

宿主机绑定的输出目录必须允许镜像默认的非 root 用户写入。
`MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT=true` 是显式的、针对单个容器的未认证任务 API
opt-in；不要将它放入 Dockerfile 或镜像的全局 ENV。优先按上例只发布到 loopback；
若要扩大暴露范围，必须放在私有网络中，或置于带认证的反向代理之后，因为 API
没有内置认证或任务所有权隔离。

### Docker Compose 配置

仓库自带的 [`docker-compose.yaml`](docker-compose.yaml) 通过两个 profile 在 NVIDIA GPU 上运行 MinerU 的 OpenAI 兼容 provider。未显式选择 profile 时，Compose 会激活两个服务；二者都绑定宿主机 `30000` 端口，因此启动前必须且只能选择一个 profile：

| Profile | 镜像 | 用途 |
| --- | --- | --- |
| `openai-server` | `alexsuntop/mineru:3.4.2` | vLLM 后端的 MinerU provider 镜像，端口 `30000`。 |
| `llama-server` | `ghcr.io/ggml-org/llama.cpp:server-cuda` | 通用 OpenAI 兼容 provider 示例，端口 `30000`；本项目不验证模型兼容性。 |

启动 vLLM 服务：

```sh
docker compose --profile openai-server up -d
```

只有在仓库外准备好模型并将其挂载到容器路径后，才启动通用 llama.cpp
服务。将准备好的 GGUF 文件放入 `./models`（或设置 `LLAMA_MODELS_DIR`），
再用 `LLAMA_MODEL` 指向容器内路径：

```sh
LLAMA_MODEL=/models/model.gguf \
docker compose --profile llama-server up -d
```

两个 server profile 默认将宿主机发布端口绑定到 `127.0.0.1`，暴露
`http://localhost:30000`（可用 `MINERU_PORT_OVERRIDE_VLLM` /
`MINERU_PORT_OVERRIDE_LLAMA` 覆盖端口）。如需其他绑定地址，必须显式设置
`MINERU_PROVIDER_BIND_HOST=<绑定地址>`；扩大暴露范围前，必须使用私有网络或带认证的
反向代理。它们映射同一宿主机端口，请只启动一个。`3.4.2` provider 镜像是独立的
provider-image baseline；兼容性文档中的 MinerU `3.4.5` 是 VLM 协议基线，不是该镜像
标签的要求。

请显式使用 `--profile openai-server` 或 `--profile llama-server`；不要在未选择 profile
时运行 `docker compose up -d`。

上面的 GHCR 镜像是已发布的 Rust API 镜像，不是包含 Python/模型的推理运行时。
Compose profile 只是独立的 provider 示例，不会准备或验证 MinerU 4.0.0a6 模型。

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
