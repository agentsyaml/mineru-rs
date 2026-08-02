# 使用说明

[简体中文](usage.md) | [English](usage.en.md)

`mineru-vlm` 将 PDF 用纯 Rust 的 Hayro 在本地以 **200 DPI** 渲染成页面图像，再调用外部、OpenAI 兼容的 MinerU VLM 服务生成版面与内容结果。它不做本地模型推理、不下载模型、不包含 `mineru-api`，且只接受 PDF。

### Rust 扩展：官方形状输出

`mineru-vlm --official-output` 是 Rust 专用的低层直接路由：它可接受 PDF 目录（递归处理），并写入 `<output>/<stem>/vlm` 的六个官方形状产物和预览。此模式下 `--base-url`、`--model` 可由 `MINERU_VL_SERVER`、`MINERU_VL_MODEL_NAME` 或单模型发现补充；默认兼容模式仍要求两者。`--batch-size` 仅可与该开关一起使用，默认 `1`，只用于本地文档分组/进度，**不是** MinerU 的 64 页处理窗口。

兼容性基线、参考套件和可复现安装方式见 [compatibility.md](compatibility.md)。该声明仅覆盖 `vlm-http-client` 的 PDF 流程，不是完整 MinerU 3.4.4 兼容性声明。

## 构建与前置条件

需要 Rust 1.89：

```sh
cargo build --release
./target/release/mineru-vlm --help
```

可执行文件为 `target/release/mineru-vlm`。渲染不依赖 PDFium 或其他本地/native PDF 运行时。

## 服务与模型

先向服务查询模型；从返回 JSON 的 `data[].id` 选择一个值作为 `--model`：

```sh
curl -H "Authorization: Bearer $MINERU_VL_API_KEY" \
  "https://<server>/v1/models"
```

`--base-url` 可传服务根地址（如 `https://<server>/`）或 `/v1` 前缀（如 `https://<server>/v1`）；程序会访问对应的 `/v1/models` 和 `/v1/chat/completions`。`--model` 必填，不能为空。

认证优先使用环境变量 `MINERU_VL_API_KEY`，也可用 `--api-key` 覆盖它。避免把密钥直接写进命令行：它可能进入 shell 历史或日志。

```sh
export MINERU_VL_API_KEY='<your-key>'
./target/release/mineru-vlm "input.pdf" \
  --base-url "https://<server>/v1" \
  --model "<model-id>" \
  --output "output"
```

若必须临时传入密钥：

```sh
./target/release/mineru-vlm "input.pdf" --base-url "https://<server>/" \
  --model "<model-id>" --api-key "<your-key>"
```

## 命令行参数

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `input` | 必填 | 输入 PDF 路径（位置参数）。 |
| `--base-url` | 必填 | OpenAI 兼容服务根地址或 `/v1` 前缀。 |
| `--model` | 必填 | `GET /v1/models` 返回的模型 ID。 |
| `--output <目录>` | `output` | 输出目录。 |
| `--api-key <密钥>` | 无 | Bearer 令牌；优先于 `MINERU_VL_API_KEY`。 |
| `--page-start <n>` | 无（从 0） | 起始页，**从 0 开始**。 |
| `--page-end <n>` | 无（到末页） | 结束页，**包含该页**。 |
| `--no-formula` | 关闭 | 不进行公式处理。 |
| `--no-table` | 关闭 | 不进行表格处理。 |
| `--no-image-analysis` | 关闭 | 不进行图像分析。 |

只处理第 0 到第 2 页，并关闭公式和图像分析：

```sh
./target/release/mineru-vlm "input.pdf" --base-url "https://<server>/v1" \
  --model "<model-id>" --page-start 0 --page-end 2 \
  --no-formula --no-image-analysis --output "result"
```

只给出 `--page-end` 时从第 0 页开始；只给出 `--page-start` 时处理到末页。起始页大于结束页、或范围超出 PDF 页数都会失败。任一错误会写入 stderr，进程以退出码 1 结束。

---

## `mineru` 规范命令（PDF / 图像 / Office）

`mineru` 是规范产品二进制，支持 PDF、图像和 Office 输入，可选 `--api-url` 远程 API 服务器模式。它不暴露本地 ML 后端；`--backend` 仅接受 `vlm-http-client`。

Office 格式转换需要 `mineru-office-convert` 辅助程序，它依赖可选的 `office` feature：

```sh
cargo build --release --features office
```

### Office helper containment

辅助程序在转换前对 OOXML 执行强制完整预检，并限制输入 32 MiB、输出 64 MiB。

| 平台 | 内存硬限制 | 其他辅助程序限制 |
| --- | --- | --- |
| Linux | `RLIMIT_AS` 1 GiB | CPU 120 秒、`NOFILE` 256、托管 180 秒 wall deadline、进程组清理 |
| Windows | Job Object 1 GiB | CPU 120 秒、托管 180 秒 wall deadline、Job tree 清理 |
| macOS | 无原生硬 RSS 上限 | 强制预检、CPU 120 秒、`NOFILE` 256、托管 180 秒 wall deadline、进程组清理 |

macOS 原生 API 没有可靠且无需 entitlement 的进程 RSS/地址空间硬限制。面向互联网、在原生 macOS 接收不可信 Office 文档的部署，必须使用外部 VM 或容器内存边界提供硬内存隔离。

### 直接 VLM 模式（默认）

不传 `--api-url` 时，`mineru` 直接调用外部 VLM 服务。服务地址和模型由 `MINERU_VL_SERVER`、`MINERU_VL_MODEL_NAME`、`MINERU_VL_API_KEY` 环境变量或 `--url` 覆盖。

```sh
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
export MINERU_VL_API_KEY="<your-key>"

./target/release/mineru -p input.pdf -o output
```

### 远程 API 服务器模式

传入 `--api-url` 时，`mineru` 将文档提交到已运行的 `mineru-api` 服务器，由服务器调用 VLM 并返回结果归档。`--url` 可覆盖单个任务的服务器端模型地址。

```sh
./target/release/mineru -p input.pdf -o output --api-url "http://127.0.0.1:8000"
```

### 命令行参数

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `-p, --path <路径>` | 必填 | 输入文件或目录（递归处理）。 |
| `-o, --output <目录>` | 必填 | 输出目录。 |
| `--api-url <URL>` | 无 | 远程 API 服务器地址；不传则直接 VLM 模式。 |
| `-m, --method <auto\|txt\|ocr>` | `auto` | 解析方法（直接模式下忽略）。 |
| `-b, --backend <vlm-http-client>` | `vlm-http-client` | 后端（仅此一个）。 |
| `--effort <medium\|high>` | `medium` | 解析力度（直接模式下忽略）。 |
| `-l, --lang <语言>` | `ch` | 语言代码。 |
| `-u, --url <URL>` | 无 | 直接模式下的 VLM 服务地址覆盖；API 模式下的任务级模型服务器覆盖。 |
| `-s, --start <n>` | `0` | 起始页，**从 0 开始**。 |
| `-e, --end <n>` | 无（到末页） | 结束页，**包含该页**。 |
| `-f, --formula <true\|false>` | `true` | 公式识别。 |
| `-t, --table <true\|false>` | `true` | 表格识别。 |
| `--image-analysis <true\|false>` | `true` | 图像分析。 |

直接模式下 `--method`、`--effort`、`--lang` 的非默认值会产生警告并被忽略。`--client-side-output-generation=true` 在 API 模式下会被拒绝。

---

## API 服务端

`mineru-api` 与 `mineru-vlm-api` 是同一个服务的两个可执行名，行为完全一致。服务本身不做本地推理，它接收文档、调用外部 VLM 服务，再把结果归档返回。

### 容器

稳定版镜像支持 `amd64` 和 `arm64`：

```sh
docker pull ghcr.io/agentsyaml/mineru-rs:latest
docker volume create mineru-output
docker run --rm -p 8000:8000 -v mineru-output:/app/output \
  -e MINERU_VL_SERVER="https://<server>" \
  -e MINERU_VL_MODEL_NAME="<model-id>" \
  -e MINERU_VL_API_KEY="<your-key>" \
  -e MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT=true \
  ghcr.io/agentsyaml/mineru-rs:latest
```

默认命令启动 API，监听 `8000`，并将任务输出写入 `/app/output`；默认使用 named volume，以保留非 root 镜像所需的目录权限。若在原生 Linux 上绑定宿主目录，需创建目录并通过 `--user "$(id -u):$(id -g)"` 以宿主用户身份运行。镜像公开绑定 API，但为避免未认证的公开解析，POST 解析必须由操作者显式启用：`-e MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT=true`。可替换默认命令运行 CLI：

```sh
docker run --rm ghcr.io/agentsyaml/mineru-rs:latest mineru --version
mkdir -p output
docker run --rm --user "$(id -u):$(id -g)" -v "$(pwd):/work" -w /work \
  -e MINERU_VL_SERVER="https://<server>" -e MINERU_VL_MODEL_NAME="<model-id>" \
  ghcr.io/agentsyaml/mineru-rs:latest mineru -p input.pdf -o output
```

### 启动

服务需要一个可用的 VLM 服务地址与模型，由 `MINERU_VL_SERVER`、`MINERU_VL_MODEL_NAME`、`MINERU_VL_API_KEY` 提供：

```sh
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
export MINERU_VL_API_KEY='<your-key>'

./target/release/mineru-api --port 8000
```

启动成功后 stderr 会输出可直接复制的服务地址与健康检查地址：

```text
server started: http://127.0.0.1:8000: health=http://127.0.0.1:8000/health
```

### 命令行参数

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `--host <IP>` | `127.0.0.1` | 监听地址。非 loopback 地址需同时设置 `MINERU_API_PUBLIC_BIND_EXPOSED`，否则启动失败。 |
| `--port <端口>` | `8000` | 监听端口。 |
| `--output-root <目录>` | `./output` | 任务输出与临时文件根目录。 |
| `--concurrency <n>` | 非 macOS `3`，macOS `1` | 同时处理的任务数。 |
| `--shutdown-on-stdin-eof` | 关闭 | stdin 关闭时优雅退出，适合由父进程托管。 |

`--output-root`、`--concurrency`、`--shutdown-on-stdin-eof` 覆盖对应环境变量；省略时保留环境变量值或上表默认值。显式传入 `--concurrency` 时 macOS 也不再强制为 1。

### 环境变量

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `MINERU_API_OUTPUT_ROOT` | `./output` | 输出根目录。 |
| `MINERU_API_MAX_CONCURRENT_REQUESTS` | 非 macOS `3`，macOS `1` | 并发任务数；非正值或非法值直接启动失败。 |
| `MINERU_API_TASK_RETENTION_SECONDS` | `86400` | 终态任务记录保留时长。 |
| `MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS` | `300` | 清理扫描间隔。 |
| `MINERU_API_PUBLIC_BIND_EXPOSED` | 关闭 | 允许监听非 loopback 地址。 |
| `MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT` | 关闭 | 公开监听时允许处理 POST 解析请求。 |
| `MINERU_API_SHUTDOWN_ON_STDIN_EOF` | 关闭 | 等价于 `--shutdown-on-stdin-eof`。 |
| `MINERU_PROCESSING_WINDOW_SIZE` | `64` | 页处理窗口。 |
| `MINERU_PDF_RENDER_THREADS` | `3` | 渲染 worker 数。 |
| `MINERU_PDF_RENDER_TIMEOUT` | `300` | 单次渲染超时秒数。 |
| `MINERU_FORMULA_ENABLE` | 开启 | 公式识别默认值。 |
| `MINERU_TABLE_ENABLE` | 开启 | 表格识别默认值。 |

布尔变量接受 `1`、`true`、`yes`、`on`（不区分大小写），其他值视为关闭。除并发外的非法数值会回落到默认值。

### HTTP 接口

`GET /health` 返回服务容量与在册任务数：

```sh
curl "http://127.0.0.1:8000/health"
```

```json
{"status":"healthy","protocol_version":2,"max_concurrent_requests":3,"processing_window_size":64,"task_count":0}
```

异步模式：`POST /tasks` 提交后立即返回 `202` 与任务快照，再轮询状态、取回结果归档。

```sh
curl -X POST "http://127.0.0.1:8000/tasks" \
  -F "files=@input.pdf" \
  -F "backend=vlm-http-client" \
  -F "response_format_zip=true"
```

```json
{"task_id":"local-0","status":"pending","backend":"vlm-http-client","file_names":["input.pdf"],"queued_ahead":0,"status_url":"http://127.0.0.1:8000/tasks/local-0","result_url":"http://127.0.0.1:8000/tasks/local-0/result","message":"Task submitted successfully"}
```

```sh
curl "http://127.0.0.1:8000/tasks/local-0"
curl -o result.zip "http://127.0.0.1:8000/tasks/local-0/result"
```

状态为 `pending`、`processing`、`completed` 或 `failed`。结果未就绪时 `GET /tasks/{id}/result` 返回 `202`，任务失败返回 `409`，未知任务返回 `404`。

同步模式：`POST /file_parse` 在同一请求内完成解析并直接流式返回结果归档，不产生可查询的任务记录。

```sh
curl -X POST "http://127.0.0.1:8000/file_parse" \
  -F "files=@input.pdf" \
  -F "backend=vlm-http-client" \
  -F "response_format_zip=true" \
  -o result.zip
```

选择建议：批量、长文档或需要进度可见性时用 `/tasks`；单个小文档、脚本内一次性调用用 `/file_parse`。

表单接受 `files` 文件部分，以及 `lang_list`、`backend`、`effort`、`parse_method`、`formula_enable`、`table_enable`、`image_analysis`、`start_page_id`、`end_page_id`、`server_url`、`response_format_zip` 和 `return_md`、`return_middle_json`、`return_model_output`、`return_content_list`、`return_images`、`return_original_file` 文本字段。字段重复、字段过多或取值非法都会被拒绝。

常见状态码：

| 状态码 | 含义 |
| --- | --- |
| `400` | multipart 非法、字段重复或过多、取值不受支持、请求 Host 非法，或公开监听下未启用解析。 |
| `408` | 请求超出 deadline。 |
| `413` | 请求体、文件或文本字段超过限制。 |
| `422` | 文件类型不受支持或文件名非法。 |
| `503` | 任务容量已满，或服务正在关闭。 |
| `409` | 任务失败或 worker 异常终止。 |

上传、排队与处理共用同一个请求 deadline，取自总解析超时（默认 24 小时），与 `MINERU_PDF_RENDER_TIMEOUT` 的单次渲染超时是两回事，服务端没有单独的环境变量可调。超时统一返回 `408` 并释放并发额度与临时目录，慢速上传不会长期占用 slot。

### 安全

- 默认只监听 loopback。绑定非 loopback 地址必须显式设置 `MINERU_API_PUBLIC_BIND_EXPOSED`；公开监听后还需 `MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT` 才会处理解析请求。
- 服务**不提供认证，也不做任务所有权隔离**：任务 ID 为顺序的 `local-N`，任何能访问服务的一方都可读取任意任务状态与结果。公网部署必须置于带认证的反向代理之后。
- 对内置的 `mineru --api-url https://...` 客户端，反向代理必须保留客户端实际发送的规范 `Host` authority：即使 `--api-url` 写有 `:443`、`:0443` 或空端口，URL/reqwest 也会省略 HTTPS 默认端口；非默认端口则规范为十进制，代理不得自行增删或改写该规范端口。后端因不存在可信代理边界而刻意忽略 `Forwarded` 和 `X-Forwarded-*`。仅在提交响应边界，若后端返回匹配该规范 authority 的 HTTP 任务链接，客户端才会在本地升级为 HTTPS 并重新执行严格同源校验；直接轮询/下载和重定向不使用此兼容规则，跨主机/端口、userinfo 和降级仍会 fail closed。若代理改写 `Host`/端口或公网路径前缀，此窄兼容规则不适用；这类部署需要外部 canonical base 配置，但目前不提供。
- 请求级 `server_url` 覆盖不会携带服务端的 API key，也不会转发任何 `Authorization` 头。
- 异步任务返回的 `status_url` / `result_url` 必须与所配置的 API 同源；重定向逐跳校验同源，异源目标不会发出请求。

## 输出

成功时，指定目录包含：

```text
output/
├── document.json          # 完整文档结果（不内嵌资产二进制数据）
├── document.md            # Markdown
├── middle.json            # 中间结构
├── content_list.json      # 内容列表
├── assets/                # 识别出的图、表、公式、图表等裁剪资产（按实际结果出现）
└── {stem}_layout.pdf      # 原 PDF 加版面块标注的预览
```

`{stem}` 是路径输入文件名去掉扩展名后的安全 stem；无安全 stem 时为 `document`。库 API 以字节传入 `PdfInput::Bytes` 时也使用 `document_layout.pdf`。输出先写入同级临时 staging 目录；完成后以重命名替换目标目录，已有目录会先作为备份，替换成功后删除备份，避免留下半写入结果。

## 库 API（最小示例）

以下示例只使用公开 API，可放入自己的 Tokio 异步程序：

```rust
use mineru::{ClientConfig, MinerUClient, ParseOptions, PdfInput, write_outputs};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ClientConfig::new("https://<server>/v1", "<model-id>")?;
    let client = MinerUClient::new(config)?;

    client.check_model().await?;
    let document = client
        .parse_pdf(PdfInput::Path("input.pdf".into()), ParseOptions::default())
        .await?;
    let outputs = write_outputs(&document, "output")?;
    println!("{}", outputs.markdown.display());
    Ok(())
}
```

需要认证时，在构造 `MinerUClient` 前为可变 `ClientConfig` 的公开 `bearer_token` 设置 `BearerToken::new(...)`。`check_model()` 会请求模型列表并确认配置的模型在其中。

## 默认资源限制

| 项目 | 默认值 |
| --- | ---: |
| PDF 大小 / 页数 | 512 MiB / 10,000 页 |
| 单页像素 / 渲染 RGB 图像 | 100,000,000 / 64 MiB |
| 响应体 / 全部资产 | 10 MiB / 1 GiB |
| 单页版面块数 / 页窗口 | 256 / 64 页 |
| 同时在途渲染图像 | 128 MiB |
| 请求并发 / 渲染 worker | 100 / 3（渲染实际最多 3） |
| 连接 / 单请求 / 总解析超时 | 10 秒 / 600 秒 / 24 小时 |

### 默认值来源与容量

- **上游锁定**：200 DPI、64 页窗口、3 个渲染 worker、VLM HTTP 最大并发 100、HTTP 请求超时 600 秒。
- **Rust 防护**：10 秒连接超时、24 小时总超时，以及页数、PDF、资产、响应、渲染图像、像素、在途图像和版面块限制。

10,000 页支持仅是高内存下的尽力而为：输入字节、最终页面结果和资产都会保留在内存中，并非无上限保证。库调用可调整公开的 `ClientConfig.limits`、`timeouts`、`request_concurrency` 和 `render_workers`，再调用 `validate()`（`MinerUClient::new` 也会验证）；应按可用 RAM 和服务端点容量配置。所有限制、并发和 worker 必须大于零；所有超时必须非零，且单请求超时不得超过总超时。

## 限制与排错

- Hayro 不支持加密 PDF；复杂/高级 PDF 效果的渲染可能与其他渲染器不同。遇到无效 PDF、页映射不一致、尺寸限制或渲染异常会明确失败，不会静默跳过。
- 预览支持页面旋转 `0/90/180/270`。其目标是可用的视觉与语义对齐；由于写入了标注且 PDF 序列化会变化，预览文件字节不等于原 PDF。其他旋转会失败。
- `401` 通常是缺失或无效的 API key；`404` 通常是 `--base-url` 路径不对。确认服务实际暴露 `/v1/models` 与 `/v1/chat/completions`。
- `check_model` 或模型检查失败时，确认 `GET /v1/models` 返回的 `data` 中含所选 ID，并检查认证和 base URL。
- `no valid layout tokens` 表示服务返回内容不含 MinerU 所需的版面 token；请选择兼容的 MinerU VLM 模型/服务，而不是普通聊天模型。
- `limit exceeded` 表示超过上表资源上限；缩小输入或在库调用中调整并验证配置。Hayro 不支持的 PDF 则需用支持该 PDF 特性的文件/渲染流程处理后再试。
