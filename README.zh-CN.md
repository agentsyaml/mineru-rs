# MinerU Rust

[English](README.md) | [简体中文](README.zh-CN.md)

用于通过外部 OpenAI 兼容的 MinerU VLM 服务解析 PDF、图像和 Office 文档的 Rust 客户端库、命令行工具和本地 API 服务。PDF 渲染使用纯 Rust；不会下载或在本地运行模型。

在 MinerU 3.4.4 VLM 范围内，MinerU Rust 是 MinerU Python SDK `vlm-http-client` 路径的可直接替代实现，可以完全替代该 VLM 工作流。本项目不实现、也不声称兼容非 VLM 后端。参见[兼容性契约](docs/compatibility.md)、[中文使用指南](docs/usage.md)和[英文使用指南](docs/usage.en.md)。

文档大小限制控制项及其 CLI/API 适用范围见使用指南。需要 Rust 1.89 或更高版本。

## Rust 库

```sh
cargo add mineru
```

```rust
use mineru::{ClientConfig, MinerUClient, ParseOptions, PdfInput};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = MinerUClient::new(ClientConfig::new(
        "https://example.test/v1",
        "model-id",
    )?)?;
    let document = client
        .parse_pdf(PdfInput::Path("input.pdf".into()), ParseOptions::default())
        .await?;
    println!("{} pages", document.pages.len());
    Ok(())
}
```

## CLI 和 API 服务端

```sh
cargo install mineru
mineru --help
```

该软件包安装 `mineru`、`mineru-api`、`mineru-vlm` 和 `mineru-vlm-api` 二进制文件。若还要安装 `mineru-office-convert` Office 转换辅助程序，请使用 `--features office` 构建：

```sh
cargo install mineru --features office
```

服务配置和完整选项请参见[中文使用指南](docs/usage.md)或[英文使用指南](docs/usage.en.md)。

## Python

wheel 支持 CPython 3.9 及更高版本：

```sh
pip install mineru-rs
```

该 wheel 安装两个等效的控制台命令 `mineru` 和 `mineru-rs`，两者均进入 `mineru_rs._cli:main`。`mineru-rs` 名称使得无需本地检出即可使用 `uvx mineru-rs --help`。若同时安装了上游 Python `mineru` 软件包，应优先使用 `mineru-rs`，因为两者都提供 `mineru` 入口点，`PATH` 中靠前的那个会生效。

```python
import mineru_rs

print(mineru_rs.canonical_stem("a bad/pdf"))
mineru_rs.validate_pdf_options(0, None, True, True, True)
```

Python 当前仅公开规范 stem 处理和 PDF 选项验证，不公开异步文档解析。发布物仅为 wheel；对于不受支持的平台或 PyPy，没有 sdist 或源代码回退方案。

## Node.js

```sh
npm install @alexsun-top/mineru
```

根软件包安装两个等效二进制文件 `mineru` 和 `mineru-rs`，两者都指向 `bin/mineru.js`，因此安装后可使用 `node_modules/.bin/mineru-rs`。六个特定平台软件包有意不附带二进制文件。若 `PATH` 上已有另一个 `mineru` 命令，应优先使用 `mineru-rs`。

```js
const mineru = require('@alexsun-top/mineru')

console.log(mineru.canonicalStem('a bad/pdf'))
mineru.validatePdfOptions(0, null, true, true, true)
```

Node.js 当前仅公开规范 stem 处理和 PDF 选项验证，不公开异步文档解析。需要 Node.js 18 或更高版本。

## 许可证

你可自行选择依据 Apache-2.0 或 MIT 许可证授权使用。
