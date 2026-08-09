# MinerU Rust

[English](README.md) | [简体中文](README.zh-CN.md)

用于通过外部 OpenAI 兼容的 MinerU VLM 服务解析 PDF、图像和 Office 文档的 Rust 客户端库、命令行工具和本地 API 服务。PDF 渲染使用纯 Rust；不会下载或在本地运行模型。

在 MinerU 3.4.4 VLM 范围内，MinerU Rust 是 MinerU Python SDK `vlm-http-client` 路径的可直接替代实现，可以完全替代该 VLM 工作流。本项目不实现、也不声称兼容非 VLM 后端。参见[兼容性契约](docs/compatibility.md)、[中文使用指南](docs/usage.md)和[英文使用指南](docs/usage.en.md)。

文档大小限制控制项及其 CLI/API 适用范围见使用指南。需要 Rust 1.89 或更高版本。

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

你的 markdown 会出现在 `out/` 中。

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
    Path("out.md").write_text(result.markdown, encoding="utf-8")


asyncio.run(main())
```

`run()` 则把完整输出树写入输出目录（见[英文使用指南](docs/usage.en.md)）。该 wheel 安装两个等效的控制台命令 `mineru` 和 `mineru-rs`；若同时安装了上游 Python `mineru` 软件包，应优先使用 `mineru-rs`，因为两者都提供 `mineru` 入口点，`PATH` 中靠前的那个会生效。发布物仅为 wheel；对于不受支持的平台或 PyPy，没有 sdist 或源代码回退方案。

## Node.js

需要 Node.js 18 或更高版本。用 `pnpm` 或 npm 安装：

```sh
pnpm add @alexsun-top/mineru
# 或：npm install @alexsun-top/mineru
```

```js
const fs = require('fs')
const mineru = require('@alexsun-top/mineru')

async function main() {
  const { markdown } = await mineru.parse({ path: 'input.pdf' })
  fs.writeFileSync('out.md', markdown)
}

main()
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

`--api-key` 可传入 Bearer 令牌，但应优先使用 `MINERU_VL_API_KEY`：命令行中的密钥会出现在进程列表中。

服务配置和完整选项请参见[中文使用指南](docs/usage.md)或[英文使用指南](docs/usage.en.md)。

## 示例

- [examples/python-uv](examples/python-uv) — 使用 `uv` 的 Python 示例
- [examples/node-pnpm](examples/node-pnpm) — 使用 `pnpm` 的 Node.js 示例

## 许可证

你可自行选择依据 Apache-2.0 或 MIT 许可证授权使用。
