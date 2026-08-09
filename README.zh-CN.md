# MinerU Rust

[English](README.md) | [简体中文](README.zh-CN.md)

使用 MinerU 将 PDF、图像和 Office 文档解析为干净的 Markdown：本项目提供 Rust 客户端库、命令行工具和本地 API 服务。PDF 渲染为纯 Rust 实现，无需 PDFium 等原生 PDF 运行时。

MinerU VLM 模型有两种运行方式：

- **远程**——让工具连接 OpenAI 兼容的 MinerU VLM 服务，无需在自己的机器上运行模型即可解析文档。
- **本地（可选）**——使用 `mineru-mistralrs` 后端，在 CPU 或 NVIDIA CUDA 上本地运行 MinerU Qwen2-VL 模型。权重（约 2.3 GB）可在首次使用时自动下载。

在 MinerU 3.4.4 VLM 范围内，MinerU Rust 是 MinerU Python SDK `vlm-http-client` 路径的可直接替代实现，可以完全替代该 VLM 工作流。本项目不实现、也不声称兼容非 VLM 后端。参见[兼容性契约](docs/compatibility.md)、[中文使用指南](docs/usage.md)和[英文使用指南](docs/usage.en.md)。文档大小限制控制项及其 CLI/API 适用范围见使用指南。

远程协议固定于 MinerU `vlm-http-client` 传输基线，因此需要兼容 MinerU 的 VLM 端点——通用聊天模型无法产出版面结果。

需要 Rust 1.89 或更高版本。

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

你的 markdown 会出现在 `out/` 中。可通过端点的 `GET /v1/models` 查询可用的模型 ID。成功后 `output/` 目录将包含 `document.md`、`document.json`、`middle.json`、`content_list.json`、裁剪后的 `assets/` 以及版面预览 `{stem}_layout.pdf`。

优先使用 `MINERU_VL_API_KEY` 环境变量，而非 `--api-key`，避免密钥进入 shell 历史记录。

### 本地：在自己机器上运行模型解析

构建本地后端并运行（CPU）：

```sh
cargo build --release --locked --features mistralrs --bin mineru-mistralrs

./target/release/mineru-mistralrs input.pdf --output output
```

默认情况下首次运行会下载固定的模型 `opendatalab/MinerU2.5-2509-1.2B`（约 2.3 GB）到 Hugging Face 缓存中，之后复用该缓存。传入 `--model-path /path/to/model` 可使用已有的本地模型目录；`--allow-download=false` 则禁止联网下载（此时必须提供 `--model-path`）。结果以 MinerU 官方输出结构写入 `output/<stem>/vlm/`。参见[本地模型与 Hugging Face 设置](#本地模型与-hugging-face-设置)、[Docker](#docker) 的 CUDA 镜像，以及[独立二进制发行](#独立二进制发行)的预编译可执行文件。

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

## CLI 和 API 服务端

```sh
cargo install mineru
mineru --help
```

该软件包安装 `mineru`、`mineru-api` 和 `mineru-vlm-api` 二进制文件。若还要安装 `mineru-office-convert` Office 转换辅助程序，请使用 `--features office` 构建：

```sh
cargo install mineru --features office
```

`mineru-api` 与 `mineru-vlm-api` 是同一个 HTTP 服务的两个名称：它接收文档、调用所配置的 VLM，并返回结果归档。该服务本身不进行本地推理。

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

### 预构建方式

- **独立二进制**——`mineru-mistralrs` 的 CPU、CUDA 与 Metal 构建，见[独立二进制发行](#独立二进制发行)。
- **Docker**——CUDA 镜像，见 [Docker](#docker)。
- **crates.io**——安装仅限远程模式的命令行工具（需 Rust 1.89+）：

  ```sh
  cargo install mineru
  cargo install mineru --features office   # 同时安装 Office 转换辅助程序
  ```

  已发布的 crate 有意不包含本地模型后端；请从源码构建 `mineru-mistralrs`（见下）、使用 CUDA 镜像，或下载独立二进制。
  注意：`cargo install mineru --features mistralrs` 虽然能编译，但会静默使用未打补丁的上游 core——请勿在已发布 crate 上使用该 flag。
- **Python**——`pip install mineru-rs`（支持 CPython 3.9+）。
- **Node.js**——`npm install @alexsun-top/mineru`（需 Node.js 18+）。

### 从源码构建

克隆仓库并构建远程 CLI/API 工具：

```sh
git clone https://github.com/agentsyaml/mineru-rs
cd mineru-rs
cargo build --release
./target/release/mineru --help
```

构建可选的本地 `mineru-mistralrs` 后端：

```sh
cargo build --release --locked --features mistralrs --bin mineru-mistralrs
```

作为库使用：`cargo add mineru`。

## 命令行用法

### 选项

本地 `mineru-mistralrs` 后端使用以下选项解析 PDF：

| 选项 | 默认值 | 说明 |
| --- | --- | --- |
| `input` | 必填 | 要解析的 PDF（位置参数）。 |
| `--output <dir>` | `output` | 结果输出目录。 |
| `--page-start <n>` | 0 | 起始解析页（从 0 开始）。 |
| `--page-end <n>` | 最后一页 | 结束解析页（含）。 |
| `--no-formula` | 关 | 跳过公式识别。 |
| `--no-table` | 关 | 跳过表格识别。 |
| `--no-image-analysis` | 关 | 跳过图像/图表分析。 |

规范的 `mineru` 命令从 `MINERU_VL_SERVER` 和 `MINERU_VL_MODEL_NAME` 环境变量读取服务地址与模型：

```sh
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
export MINERU_VL_API_KEY="<your-key>"
mineru -p input.pdf -o output
```

使用本地后端只处理第 0–2 页，并关闭公式与图像分析：

```sh
mineru-mistralrs input.pdf --page-start 0 --page-end 2 \
  --no-formula --no-image-analysis --output output
```

### 输出目录

- `mineru`（远程）直接写入 `output/`：`document.md`、`document.json`、`middle.json`、`content_list.json`、裁剪后的 `assets/` 以及版面预览 `{stem}_layout.pdf`。
- `mineru-mistralrs`（本地）以 MinerU 官方输出结构写入 `output/<stem>/vlm/`：`{stem}.md`、`{stem}_middle.json`、`{stem}_model.json`、`{stem}_content_list.json`、`{stem}_content_list_v2.json`、`{stem}_layout.pdf` 和 `images/`。

`{stem}` 为输入文件名去掉扩展名后的部分。

### API 服务

`mineru-api` 与 `mineru-vlm-api` 是同一个 HTTP 服务的两个名称：它接收文档、调用所配置的 VLM，并返回结果归档。该服务本身不进行本地推理。API 模式的提交流程见 [CLI 和 API 服务端](#cli-和-api-服务端)。

## 本地模型与 Hugging Face 设置

本地 `mineru-mistralrs` 后端通过命令行参数从两个来源之一加载 Qwen2-VL MinerU 模型：

| 参数 | 默认值 | 作用 |
| --- | --- | --- |
| `--model-path <目录>` | 无 | 使用本地模型目录。该设置始终优先，且绝不会回退到联网下载。 |
| `--allow-download[=<布尔值>]` | `true` | 未指定 `--model-path` 时，首次使用将 `opendatalab/MinerU2.5-2509-1.2B`（约 2.3 GB）下载到 Hugging Face 缓存。 |

`--allow-download` 接受显式布尔值（`--allow-download=false` 禁止下载；此时必须提供 `--model-path`）。旧的 `MINERU_VL_MODEL_DIR` 与 `MINERU_VL_AUTO_DOWNLOAD` 环境变量仍被支持，命令行参数优先。

本地模型目录必须包含 `config.json`（Qwen2-VL 架构）、`tokenizer.json`、`preprocessor_config.json` 和 `model.safetensors`；该目录会在任何其他操作之前被校验。

同时适用于下载的 Hugging Face 设置：

| 变量 | 作用 |
| --- | --- |
| `HF_HOME` | 模型缓存存放位置。 |
| `HF_TOKEN` | Hugging Face 访问令牌，用于受限或私有仓库。 |
| `HF_HUB_OFFLINE=1` | 强制完全离线，仅使用本地缓存。 |

## 五个二进制文件

| 二进制 | 用途 |
| --- | --- |
| `mineru` | 主命令行工具：支持 PDF、图像和 Office 文档，可直接对接 VLM，也可通过 `mineru-api` 服务。 |
| `mineru-api` | HTTP API 服务（见上文）。 |
| `mineru-vlm-api` | 同一服务的另一名称。 |
| `mineru-office-convert` | Office（.docx/.pptx/.xlsx）→ PDF 转换辅助程序，供 `mineru` 使用；需以 `--features office` 构建。 |
| `mineru-mistralrs` | 本地 PDF 解析，使用 Qwen2-VL MinerU 模型；从源码以 `--features mistralrs` 构建，或随 CUDA 镜像与独立二进制提供。 |

## Docker

### CUDA 镜像（本地推理）

`ghcr.io/agentsyaml/mineru-rs-cuda:latest-sm80` 提供 `mineru-mistralrs`，可在 NVIDIA GPU 上进行本地解析。该 tag 面向计算能力 8.0（Ampere 级）GPU，并非通用的全 GPU 镜像。镜像 ENTRYPOINT 为 `mineru-mistralrs`，镜像名之后的参数直接传给 CLI：

```sh
mkdir -p output .hf-cache
docker run --rm --gpus all --user "$(id -u):$(id -g)" \
  -v "$(pwd):/work" -w /work \
  -e HF_HOME=/work/.hf-cache \
  ghcr.io/agentsyaml/mineru-rs-cuda:latest-sm80 \
  input.pdf --output output
```

对于其他 GPU，请按你的 GPU 计算能力自行构建镜像（例如 RTX 40 系列使用 `89`）：

```sh
docker build --platform linux/amd64 -f Dockerfile.cuda --build-arg CUDA_COMPUTE_CAP=89 -t mineru-rs-cuda .
docker run --rm --gpus all --user "$(id -u):$(id -g)" \
  -v "$(pwd):/work" -w /work \
  -e HF_HOME=/work/.hf-cache \
  mineru-rs-cuda input.pdf --output output
```

`.hf-cache` 绑定目录会跨多次运行保留已下载的权重。也可以使用挂载到 `/data` 的命名卷；CUDA 镜像默认将该路径作为 `HF_HOME`。

不再发布 CPU Docker 镜像；CPU 后端以独立二进制形式发行（见下）。

## 独立二进制发行

预编译的 `mineru-mistralrs` 可执行文件作为 `mineru-mistralrs-{variant}-{rust-target}.tar.gz` 附加到每个 GitHub Release；每个压缩包只含单个可执行文件，无需 npm 或 Python 包装。

| 变体 | Rust 目标 | 平台 | 说明 |
| --- | --- | --- | --- |
| `cpu` | `x86_64-unknown-linux-gnu` | Linux，amd64 | CPU 推理。 |
| `cpu` | `aarch64-unknown-linux-gnu` | Linux，arm64 | CPU 推理。 |
| `cpu` | `aarch64-apple-darwin` | macOS，Apple Silicon | CPU 推理。 |
| `metal` | `aarch64-apple-darwin` | macOS，Apple Silicon | Metal 加速推理；需 macOS 13+ 与 Apple Silicon。 |
| `cuda` | `x86_64-unknown-linux-gnu` | Linux，amd64 | CUDA 推理；需 NVIDIA 驱动（`libcuda.so.1`）与计算能力 8.0（Ampere 级）或更新的 GPU。 |

下载、解压并运行：

```sh
curl -LO https://github.com/agentsyaml/mineru-rs/releases/latest/download/mineru-mistralrs-cpu-x86_64-unknown-linux-gnu.tar.gz
tar xzf mineru-mistralrs-cpu-x86_64-unknown-linux-gnu.tar.gz
./mineru-mistralrs input.pdf --output output
```

每个 Release 还会附加一份覆盖其全部产物的 `SHA256SUMS` 文件，可用于在运行前校验压缩包。

## 示例

- [examples/python-uv](examples/python-uv) — 使用 `uv` 的 Python 示例
- [examples/node-pnpm](examples/node-pnpm) — 使用 `pnpm` 的 Node.js 示例

## 构建与测试

```sh
cargo build --release
cargo test

# 本地后端单元测试（真实模型测试被忽略，除非
# MINERU_VL_MODEL_DIR 指向本地权重）
cargo test --features mistralrs --lib
```

## 许可证

MIT OR Apache-2.0。从 Hugging Face 下载的模型权重受其模型卡所示许可证的约束。
