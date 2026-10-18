# MinerU Rust

[English](README.md) | 中文

将 PDF、图像和 Office 文档解析为 Markdown。本项目提供 Rust 库、CLI 和 HTTP API
服务，PDF 渲染由 Rust 实现。旧版 CLI/API 与 Python/Node `run`/`parse` 使用远程
MinerU 兼容 VLM（MinerU 3.4.5 协议基线）；rich SDK 则使用用户另行准备的
MinerU 4.0.12 / DocVortex 0.5.16 Python worker。

选择使用方式：

- **Python / Node**：安装已发布的软件包；`parse()` 返回 Markdown，`run()` 写出完整结果目录。
- **官方 rich SDK**：Rust、Python 或 Node SDK 需另行准备 Python 3.10–3.14 worker（`mineru==4.0.12`、`docvortex==0.5.16`），支持原生格式；详见[SDK 指南](docs/official-sdk.md)。
- **Rust CLI / API 服务**：从准备好的源码检出构建。根工作区 Cargo 补丁不会传递给消费者，因此该 crate 不能从 crates.io 安装。
- **本地转换**：CLI `--backend local` 通过内置 Rust helper 支持干净文本 PDF 和部分旧格式；只输出 Markdown，不生成版面 JSON，也不调用 VLM。

当前源码检出目标版本为 0.3.1。安装指定版本前，请查看
[发布指南](docs/releasing.md#current-release-boundary)确认软件包和发布可用性。

详细说明：[使用指南](docs/usage.md)、[兼容性](docs/compatibility.md)、
[SDK](docs/official-sdk.md) 和[官方 HTTP API](docs/official-http.md)。

## 快速开始

需要 CPython 3.9+ 或 Node.js 18+，并配置 MinerU 兼容的 VLM 服务。
通用聊天服务无法提供版面解析。

| 变量 | 含义 | 示例 |
| --- | --- | --- |
| `MINERU_VL_SERVER` | VLM 服务基础 URL | `https://host/v1` |
| `MINERU_VL_MODEL_NAME` | 模型 ID | `model-id` |
| `MINERU_VL_API_KEY` | Bearer 令牌 | `your-key` |

```bash
export MINERU_VL_SERVER="https://host/v1"
export MINERU_VL_MODEL_NAME="model-id"
# 若服务需要密钥，用无回显输入，避免密钥进入 shell 历史：
read -rs -p "VLM API key: " MINERU_VL_API_KEY; export MINERU_VL_API_KEY
printf '\n'
```

Python CLI：安装已发布 wheel 后运行：

```sh
python -m pip install mineru-rs
mineru-rs -p input.pdf -o out/
```

Node.js：查询 npm 上的当前版本并调用其 `mineru-rs` CLI，无需加入项目依赖：

```bash
version="$(npm view @alexsun-top/mineru version)"
npx --yes --package="@alexsun-top/mineru@$version" -- mineru-rs -p input.pdf -o out/
```

完整结果目录会写入 `out/`。若只需要内存中的 Markdown，可在应用中使用
`parse()`；`run()` 会写出完整结果目录。

优先使用 `MINERU_VL_API_KEY` 环境变量，而非 `--api-key`，避免密钥进入 shell 历史记录。

## Python

在 CPython 3.9+ 上安装 `mineru-rs`。`parse()` 返回 Markdown，`run()` 写出
完整结果目录。已发布 wheel 不含 Rust Office 转换 helper。详见
[uv 示例](examples/python-uv/README.md)和[SDK 指南](docs/official-sdk.md)。

## Node.js

在 Node.js 18+ 上安装 `@alexsun-top/mineru`。`parse()` 返回 Markdown，
`run()` 写出完整结果目录。已发布包不含 Rust Office 转换 helper。详见
[pnpm 示例](examples/node-pnpm/README.md)和[SDK 指南](docs/official-sdk.md)。

## CLI 和 API 服务端

Rust 1.89+ CLI 可从准备好的源码检出安装；`office` 和 `legacy-office` 是可选
feature。准备与安装步骤见[源码构建](#从源码构建)。

```sh
cargo install --path . --locked --features office,legacy-office
```

`backend=local` 通过内置 helper 将干净文本 PDF 和部分旧格式转换为 Markdown；
不运行模型，不生成版面 JSON。不确定的 PDF 会失败。完整输入及 backend 说明见
[使用指南](docs/usage.md)和[兼容性](docs/compatibility.md)。

`mineru-api` 调用配置的 VLM 并返回结果归档，不执行本地推理：

```bash
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
# 若服务需要密钥，用无回显输入，避免密钥进入 shell 历史：
read -rs -p "VLM API key: " MINERU_VL_API_KEY; export MINERU_VL_API_KEY
printf '\n'

mineru-api --port 8000
```

API 没有内置认证或任务所有权。请仅绑定 loopback，或置于带认证的反向代理后；
端点和完整选项见[使用指南](docs/usage.md)。

## 从源码构建

需要 Rust 1.89+ 和 Python 3.9+。在工作区运行任何 Cargo 命令前，先准备依赖：

```sh
git clone https://github.com/agentsyaml/mineru-rs
cd mineru-rs
python3 tools/deps/prepare_office.py prepare
python3 tools/deps/prepare_office.py verify
cargo build --release --locked --features office,legacy-office
./target/release/mineru --help
```

`prepare` 会下载带哈希校验的源码归档；`verify` 离线检查生成的
`.deps/office-xml041/`。不要手工修改该目录。安装 CLI：
`cargo install --path . --locked --features office,legacy-office`。

## 示例

- [Python + uv](examples/python-uv/README.md)
- [Node.js + pnpm](examples/node-pnpm/README.md)

## 许可证

MIT OR Apache-2.0。从 Hugging Face 下载的模型权重受其模型卡所示许可证的约束。
