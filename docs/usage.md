# 使用说明

[简体中文](usage.md) | [English](usage.en.md)

`mineru-vlm` 将 PDF 用纯 Rust 的 Hayro 在本地以 **200 DPI** 渲染成页面图像，再调用外部、OpenAI 兼容的 MinerU VLM 服务生成版面与内容结果。它不做本地模型推理、不下载模型、不包含 `mineru-api`，且只接受 PDF。

### Rust 扩展：官方形状输出

`mineru-vlm --official-output` 是 Rust 专用的低层直接路由：它可接受 PDF 目录（递归处理），并写入 `<output>/<stem>/vlm` 的六个官方形状产物和预览。此模式下 `--base-url`、`--model` 可由 `MINERU_VL_SERVER`、`MINERU_VL_MODEL_NAME` 或单模型发现补充；默认兼容模式仍要求两者。`--batch-size` 仅可与该开关一起使用，省略时使用官方路由的编译默认值 32。它是每页真实的语义推理请求准入（推理批大小），**不是**输入文档分组，也**不是** MinerU 的 64 页处理窗口；页并发（`--page-concurrency`）与处理窗口（`--processing-window-size`）是相互独立的轴。

兼容性基线、参考套件和可复现安装方式见 [compatibility.md](compatibility.md)。该声明仅覆盖 `vlm-http-client` 的 PDF 流程，不是完整 MinerU 3.4.4 兼容性声明。

## 构建与前置条件

需要 Rust 1.89：

```sh
cargo build --release
./target/release/mineru-vlm --help
```

可执行文件为 `target/release/mineru-vlm`。渲染不依赖 PDFium 或其他本地/native PDF 运行时。

## 快速开始

用三个环境变量配置 VLM 服务：

| 变量 | 含义 | 示例 |
| --- | --- | --- |
| `MINERU_VL_SERVER` | VLM 服务基础 URL | `https://host/v1` |
| `MINERU_VL_MODEL_NAME` | 模型 ID | `model-id` |
| `MINERU_VL_API_KEY` | Bearer 令牌 | `your-key` |

然后解析 PDF：

```sh
mineru -p input.pdf -o out/
```

你的 markdown 会出现在 `out/` 中。也可用 `--api-key` 传入 Bearer 令牌，但应优先使用环境变量：命令行中的密钥会出现在进程列表中。这些变量同样列于下文的[环境变量表](#环境变量)。

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
| `-f, --formula <true\|false>` | `true` | 公式识别。显式布尔值，优先级 `CLI > MINERU_FORMULA_ENABLE > 默认值`。 |
| `-t, --table <true\|false>` | `true` | 表格识别。显式布尔值，优先级 `CLI > MINERU_TABLE_ENABLE > 默认值`。 |
| `--image-analysis <true\|false>` | `true` | 图像分析。显式布尔值，优先级 `CLI > MINERU_IMAGE_ANALYSIS_ENABLE > 默认值`。 |
| `--log-level <级别>` | `info` | 日志级别：`trace`、`debug`、`info`、`success`、`warning`、`error`、`critical`。覆盖 `MINERU_LOG_LEVEL`。 |
| `--processing-window-size <n>` | `64` | 页处理窗口。覆盖 `MINERU_PROCESSING_WINDOW_SIZE`。 |
| `--page-concurrency <n>` | `4` | 官方页准入并发（任意正整数；仍受窗口与 HTTP 并发下限约束）。覆盖 `MINERU_OFFICIAL_PAGE_CONCURRENCY`。 |
| `--render-workers <n>` | `3` | 渲染 worker 数；实际值受可用并行度与所选页数约束，不再被 3 封顶。覆盖 `MINERU_PDF_RENDER_THREADS`。 |
| `--render-timeout-seconds <n>` | `300` | 单次渲染超时。覆盖 `MINERU_PDF_RENDER_TIMEOUT`。 |
| `--batch-size <n>` | `32` | 每页语义推理请求准入（推理批大小），区别于页并发与处理窗口。覆盖 `MINERU_BATCH_SIZE`。 |
| `--total-deadline-seconds <n>` | `86400` | 单文档总 deadline。覆盖 `MINERU_TOTAL_DEADLINE_SECONDS`。 |
| `--max-pdf-bytes <n>` | `536870912` | 常驻源 PDF 上限。覆盖 `MINERU_MAX_PDF_BYTES`。 |
| `--max-pages <n>` | `10000` | 每文档最大选中页数。覆盖 `MINERU_MAX_PAGES`。 |
| `--max-page-pixels <n>` | `100000000` | 单页像素上限。覆盖 `MINERU_MAX_PAGE_PIXELS`。 |
| `--max-rendered-image-bytes <n>` | `67108864` | 单次渲染 RGB 上限。覆盖 `MINERU_MAX_RENDERED_IMAGE_BYTES`。 |
| `--max-in-flight-image-bytes <n>` | `134217728` | 在途 RGB 预算。覆盖 `MINERU_MAX_IN_FLIGHT_IMAGE_BYTES`。 |
| `--max-raw-output-bytes <n>` | `134217728` | 单文档原始输出预算。覆盖 `MINERU_MAX_RAW_OUTPUT_BYTES`。 |
| `--max-layout-blocks-per-page <n>` | `256` | 单页版面块上限。覆盖 `MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE`。 |
| `--max-semantic-requests-per-page <n>` | `128` | 单页语义请求上限。覆盖 `MINERU_MAX_SEMANTIC_REQUESTS_PER_PAGE`。 |
| `--max-encoded-request-bytes <n>` | `16777216` | 编码请求上限。覆盖 `MINERU_MAX_ENCODED_REQUEST_BYTES`。 |
| `--max-encoded-batch-bytes <n>` | `67108864` | 编码批上限。覆盖 `MINERU_MAX_ENCODED_BATCH_BYTES`。 |
| `--max-total-asset-bytes <n>` | `1073741824` | 全部资产上限。覆盖 `MINERU_MAX_TOTAL_ASSET_BYTES`。 |
| `--max-staged-text-bytes <n>` | `268435456` | 暂存文本上限。覆盖 `MINERU_MAX_STAGED_TEXT_BYTES`。 |

所有数值 flag 均为严格解析：非法、非有限、不应为零却为零、溢出或平台不可表示的值会在任何网络/输出工作前失败。每个旋钮的优先级均为 `CLI > 环境变量 > 编译默认值`。

VLM 传输旋钮（每个都有对应的环境拼写）：

| Flag | 默认值 | 覆盖 |
| --- | ---: | --- |
| `--http-max-concurrency <n>` | `100` | `MINERU_VLM_HTTP_CONCURRENCY` |
| `--http-timeout-seconds <n>` | `600` | `MINERU_VLM_HTTP_TIMEOUT` |
| `--connect-timeout-seconds <n>` | `10` | `MINERU_VLM_CONNECT_TIMEOUT` |
| `--http-max-keepalive-connections <n>` | `20` | `MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS` |
| `--http-keepalive-expiry-seconds <n>` | `5` | `MINERU_VLM_HTTP_KEEPALIVE_EXPIRY` |
| `--http-max-retries <n>` | `3` | `MINERU_VLM_HTTP_MAX_RETRIES` |
| `--http-retry-backoff-factor <f>` | `0.5` | `MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR` |
| `--max-remote-image-bytes <n>` | `33554432` | `MINERU_VLM_MAX_IMAGE_BYTES` |
| `--max-decoded-pixels <n>` | `100000000` | `MINERU_VLM_MAX_DECODED_PIXELS` |
| `--max-images-per-request <n>` | `64` | `MINERU_VLM_MAX_IMAGES_PER_REQUEST` |
| `--max-redirects <n>` | `3` | `MINERU_VLM_MAX_REDIRECTS` |
| `--http-max-response-bytes <n>` | `10485760` | `MINERU_VLM_HTTP_MAX_RESPONSE_BYTES` |
| `--vlm-debug <true\|false>` | `false` | 在 VLM 请求体中发送 `vllm_xargs.debug`。覆盖 `MINERU_VL_DEBUG_ENABLE`。 |

诊断/人类输出截断上限保持编译固定、不可配置。现有 `--max-input-bytes`、`--max-encoded-document-bytes`、`--max-output-bytes` 三组不变。

直接模式下 `--method`、`--effort`、`--lang` 的非默认值会产生警告并被忽略。`--client-side-output-generation=true` 在 API 模式下会被拒绝。

API 模式下本地 VLM 传输旋钮（`--page-concurrency`、`--processing-window-size`、`--render-*`、`--batch-size`、全部 `--http-*`/`--max-remote-image-bytes`/`--max-decoded-pixels`/`--max-images-per-request`/`--max-redirects`/`--http-max-response-bytes`/`--vlm-debug` 及其环境拼写）会显式报错，因为远程服务器执行解析、这些配置不会有任何消费者；`MINERU_VL_SERVER` 在未传 `--url` 时作为任务级 `server_url` 提交。

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
| `--concurrency <n>` | `3` | 同时处理的任务数。 |
| `--shutdown-on-stdin-eof` | 关闭 | stdin 关闭时优雅退出，适合由父进程托管。 |

`--output-root`、`--concurrency`、`--shutdown-on-stdin-eof` 覆盖对应环境变量；省略时保留环境变量值或上表默认值。所有平台均接受显式值，不再有 macOS 并发下限。

### 环境变量

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `MINERU_API_OUTPUT_ROOT` | `./output` | 输出根目录。 |
| `MINERU_API_MAX_CONCURRENT_REQUESTS` | `3` | 并发任务数；非正值或非法值直接启动失败。 |
| `MINERU_API_TASK_RETENTION_SECONDS` | `86400` | 终态任务记录保留时长。 |
| `MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS` | `300` | 清理扫描间隔。 |
| `MINERU_API_PUBLIC_BIND_EXPOSED` | 关闭 | 允许监听非 loopback 地址。 |
| `MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT` | 关闭 | 公开监听时允许处理 POST 解析请求。 |
| `MINERU_API_SHUTDOWN_ON_STDIN_EOF` | 关闭 | 等价于 `--shutdown-on-stdin-eof`。 |
| `MINERU_API_RECORD_CAP` | `32` | 并发任务记录上限。 |
| `MINERU_API_FILE_CAP` | `536870912` | 单文件上传字节上限。 |
| `MINERU_API_BODY_CAP` | `537001984` | multipart 请求体字节上限。 |
| `MINERU_API_TEXT_CAP` | `65536` | 单个表单文本字段字节上限。 |
| `MINERU_API_TEXT_TOTAL_CAP` | `262144` | 表单文本合计字节上限。 |
| `MINERU_API_FORM_FIELDS_CAP` | `32` | multipart 表单字段数量上限。 |
| `MINERU_OFFICE_INPUT_BYTES` | `33554432` | Office 辅助进程输入字节上限（子进程环境）。 |
| `MINERU_OFFICE_OUTPUT_BYTES` | `67108864` | Office 辅助进程输出 PDF 字节上限。 |
| `MINERU_OFFICE_STDERR_BYTES` | `4096` | Office 辅助进程 stderr 诊断上限。 |
| `MINERU_OFFICE_WALL_SECONDS` | `180` | Office 辅助进程托管 wall 时间上限。 |
| `MINERU_OFFICE_CPU_SECONDS` | `120` | Office 辅助进程 CPU 秒数 rlimit。 |
| `MINERU_OFFICE_NOFILE` | `256` | Office 辅助进程 NOFILE rlimit。 |
| `MINERU_OFFICE_ADDRESS_SPACE_BYTES` | `1073741824` | Office 辅助进程地址空间 rlimit（Linux）。 |
| `MINERU_OFFICE_ACTIVE_PROCESS_LIMIT` | `8` | Office 辅助进程 Windows 作业活动进程上限。 |
| `MINERU_OFFICE_PROCESS_MEMORY_BYTES` | `1073741824` | Office 辅助进程 Windows 单进程内存上限。 |
| `MINERU_OFFICE_JOB_MEMORY_BYTES` | `1073741824` | Office 辅助进程 Windows 作业内存上限。 |
| `MINERU_OFFICE_PROCESS_TIME_SECONDS` | `120` | Office 辅助进程 Windows 单进程用户时间。 |
| `MINERU_OFFICE_JOB_TIME_SECONDS` | `120` | Office 辅助进程 Windows 作业用户时间。 |
| `MINERU_OOXML_ARCHIVE_BYTES` | `536870912` | OOXML 预检归档字节上限。 |
| `MINERU_OOXML_EXPANDED_BYTES` | `268435456` | OOXML 预检解压字节上限。 |
| `MINERU_OOXML_XML_ENTRY_BYTES` | `8388608` | OOXML 单个 XML 条目字节上限。 |
| `MINERU_OOXML_XML_TOTAL_BYTES` | `33554432` | OOXML XML 合计字节上限。 |
| `MINERU_OOXML_RATIO` | `500` | OOXML 条目压缩比上限。 |
| `MINERU_OOXML_XML_DEPTH` | `128` | OOXML XML 深度上限。 |
| `MINERU_OOXML_XML_EVENTS` | `100000` | OOXML XML 事件数上限。 |
| `MINERU_OOXML_XML_ATTRIBUTES` | `256` | OOXML 单元素属性数上限。 |
| `MINERU_OOXML_XML_NAMESPACES` | `256` | OOXML 单元素命名空间数上限。 |
| `MINERU_PROCESSING_WINDOW_SIZE` | `64` | 页处理窗口。 |
| `MINERU_OFFICIAL_PAGE_CONCURRENCY` | `4` | 官方直接路由页并发；整数范围 1 到 8（`2` 为低内存回退值）。 |
| `MINERU_PDF_RENDER_THREADS` | `3` | 渲染 worker 数。 |
| `MINERU_PDF_RENDER_TIMEOUT` | `300` | 单次渲染超时秒数。 |
| `MINERU_FORMULA_ENABLE` | 开启 | 公式识别默认值（严格 `true`/`false`，不区分大小写）。 |
| `MINERU_TABLE_ENABLE` | 开启 | 表格识别默认值（严格 `true`/`false`）。 |
| `MINERU_IMAGE_ANALYSIS_ENABLE` | 开启 | 图像分析默认值（严格 `true`/`false`）。 |
| `MINERU_LOG_LEVEL` | `info` | 日志级别；`critical` 静默进度输出。 |
| `MINERU_PROCESSING_WINDOW_SIZE` | `64` | 页处理窗口。 |
| `MINERU_OFFICIAL_PAGE_CONCURRENCY` | `4` | 官方页准入并发（任意正整数）。 |
| `MINERU_PDF_RENDER_THREADS` | `3` | 渲染 worker 数。 |
| `MINERU_PDF_RENDER_TIMEOUT` | `300` | 单次渲染超时秒数。 |
| `MINERU_BATCH_SIZE` | `32` | 每页语义推理请求准入。 |
| `MINERU_TOTAL_DEADLINE_SECONDS` | `86400` | 单文档总 deadline。 |
| `MINERU_MAX_PDF_BYTES` | `536870912` | 常驻源 PDF 上限。 |
| `MINERU_MAX_PAGES` | `10000` | 每文档最大选中页数。 |
| `MINERU_MAX_PAGE_PIXELS` | `100000000` | 单页像素上限。 |
| `MINERU_MAX_RENDERED_IMAGE_BYTES` | `67108864` | 单次渲染 RGB 上限。 |
| `MINERU_MAX_IN_FLIGHT_IMAGE_BYTES` | `134217728` | 在途 RGB 预算。 |
| `MINERU_MAX_RAW_OUTPUT_BYTES` | `134217728` | 单文档原始输出预算。 |
| `MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE` | `256` | 单页版面块上限。 |
| `MINERU_MAX_SEMANTIC_REQUESTS_PER_PAGE` | `128` | 单页语义请求上限。 |
| `MINERU_MAX_ENCODED_REQUEST_BYTES` | `16777216` | 编码请求上限。 |
| `MINERU_MAX_ENCODED_BATCH_BYTES` | `67108864` | 编码批上限。 |
| `MINERU_MAX_TOTAL_ASSET_BYTES` | `1073741824` | 全部资产上限。 |
| `MINERU_MAX_STAGED_TEXT_BYTES` | `268435456` | 暂存文本上限。 |
| `MINERU_VLM_HTTP_CONCURRENCY` | `100` | VLM HTTP 并发。 |
| `MINERU_VLM_HTTP_TIMEOUT` | `600` | VLM HTTP 请求超时秒数。 |
| `MINERU_VLM_CONNECT_TIMEOUT` | `10` | 连接超时秒数。 |
| `MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS` | `20` | keepalive 连接池大小。 |
| `MINERU_VLM_HTTP_KEEPALIVE_EXPIRY` | `5` | keepalive 过期秒数。 |
| `MINERU_VLM_HTTP_MAX_RETRIES` | `3` | HTTP 重试次数。 |
| `MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR` | `0.5` | 重试退避因子。 |
| `MINERU_VLM_MAX_IMAGE_BYTES` | `33554432` | 远程图像字节上限。 |
| `MINERU_VLM_MAX_DECODED_PIXELS` | `100000000` | 解码像素上限。 |
| `MINERU_VLM_MAX_IMAGES_PER_REQUEST` | `64` | 每请求图像数上限。 |
| `MINERU_VLM_MAX_REDIRECTS` | `3` | 重定向上限。 |
| `MINERU_VLM_HTTP_MAX_RESPONSE_BYTES` | `10485760` | VLM HTTP 响应上限。 |
| `MINERU_VL_DEBUG_ENABLE` | 关闭 | VLM 请求调试标记（严格 `true`/`false`）。 |
| `MINERU_VL_SERVER` | 无 | VLM 服务基础 URL（如 `https://host/v1`）；`mineru` 直接模式与 `mineru-api` 必填。 |
| `MINERU_VL_MODEL_NAME` | 无 | 模型 ID；`mineru` 直接模式与 `mineru-api` 必填。 |
| `MINERU_VL_API_KEY` | 无 | VLM 服务的 Bearer 令牌。 |

对规范 CLI，每个数值与布尔变量均为严格解析：布尔只接受不区分大小写的 `true`/`false`（`1`、`yes`、`on` 会报错，不再静默视为关闭）；数值的非法、非有限、不应为零却为零、溢出或平台不可表示的值会在任何网络/输出工作前失败，不再回落到默认值。（服务启动路径在服务车道落地前保留旧的回落行为；其并发配置对非正值仍然启动失败。）

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

## Python 和 Node.js 绑定

`mineru-rs` Python 软件包和 `@alexsun-top/mineru` Node.js 软件包封装同一个解析器。两者都提供 `parse()`（在内存中返回 markdown）和 `run()`（写入完整输出树）。

### Python

```sh
uv add mineru-rs   # 或：pip install mineru-rs
```

```python
import asyncio
from pathlib import Path

import mineru_rs


async def main() -> None:
    result = await mineru_rs.parse("input.pdf")
    Path("out.md").write_text(result.markdown, encoding="utf-8")


asyncio.run(main())
```

`parse()` 在内存中返回 markdown 字符串，由调用方决定如何持久化。`run()` 把完整输出树（markdown、JSON、资产）写入输出目录：

```python
import asyncio

import mineru_rs


async def main() -> None:
    await mineru_rs.run("input.pdf", "out/")


asyncio.run(main())
```

两者都接受与 CLI 相同的关键字选项：`api_url`、`method`（`auto`/`txt`/`ocr`）、`backend`（`vlm-http-client`）、`effort`（`medium`/`high`）、`lang`（默认 `ch`）、`url`（直接 VLM 服务）、`start`、`end`、`formula`、`table`、`image_analysis` 和 `client_side_output_generation`。

### Node.js

```sh
pnpm add @alexsun-top/mineru   # 或：npm install @alexsun-top/mineru
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

`parse()` 解析为 `{ markdown, warnings }`；markdown 字符串在内存中返回，由调用方决定如何持久化。`run()` 写入完整输出树并解析为 `{ warnings }`：

```js
const mineru = require('@alexsun-top/mineru')

mineru.run({ path: 'input.pdf', output: 'out/' }).then(({ warnings }) => {
  if (warnings.length) console.warn(warnings)
})
```

选项以 camelCase 命名镜像 CLI：`apiUrl`、`method`、`backend`、`effort`、`lang`、`url`、`start`、`end`、`formula`、`table`、`imageAnalysis` 和 `clientSideOutputGeneration`。

## 默认资源限制

### 文档大小控制

`--max-input-bytes` / `MINERU_MAX_INPUT_BYTES`、`--max-encoded-document-bytes` / `MINERU_MAX_ENCODED_DOCUMENT_BYTES` 和 `--max-output-bytes` / `MINERU_MAX_OUTPUT_BYTES` 接受无符号十进制字节数（允许空白和 `_`）。优先级为 CLI、环境变量、编译默认值：输入 4_293_918_719 字节、编码文档 8 GiB、输出 8 GiB。显式的非法、零、溢出或平台不可表示值会失败；不再存在任意硬上限——配置值本身作为策略使用，而不会被夹紧到另一个常数。

这些是磁盘/文档总量而非常驻内存分配：解析后的 PDF 和当前 PDF 压缩器会在 `lopdf` 加载前拒绝超过常驻上限（`--max-pdf-bytes` / `MINERU_MAX_PDF_BYTES`，默认 512 MiB）的源 PDF，单个 VLM 响应仍限制为 10 MiB（`--http-max-response-bytes`）。编码策略应在 `mineru-vlm-api` 配置；规范远程模式会拒绝编码覆盖项。普通遗留 `mineru-vlm` 保持常驻解析器限制，编码文档覆盖项需要 `--official-output`。

| 项目 | 默认值 |
| --- | ---: |
| PDF 大小 / 页数 | 512 MiB / 10,000 页 |
| 单页像素 / 渲染 RGB 图像 | 100,000,000 / 64 MiB |
| 响应体 / 全部资产 | 10 MiB / 1 GiB |
| 单页版面块数 / 页窗口 | 256 / 64 页 |
| 单页语义请求数 / 推理批 | 128 / 32 |
| 同时在途渲染图像 | 128 MiB |
| 请求并发 / 渲染 worker | 100 / 3（worker 还受 CPU 与所选页数约束，不再封顶 3） |
| 官方页准入并发 | 4（无固定上限） |
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
