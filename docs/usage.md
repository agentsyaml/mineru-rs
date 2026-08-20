# 使用说明

[简体中文](usage.md) | [English](usage.en.md)

兼容性基线与可复现安装方式见 [compatibility.md](compatibility.md)。该声明仅覆盖 `vlm-http-client` 的 PDF 流程，不是完整 MinerU 3.4.5 兼容性声明。

## 构建与前置条件

需要 Rust 1.89：

```sh
cargo build --release
./target/release/mineru --help
```

可执行文件为 `target/release/mineru`。渲染不依赖 PDFium 或其他本地/native PDF 运行时。

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

先向服务查询模型；从返回 JSON 的 `data[].id` 选择一个值，设为 `MINERU_VL_MODEL_NAME`：

```sh
curl -H "Authorization: Bearer $MINERU_VL_API_KEY" \
  "https://<server>/v1/models"
```

直接模式下 `mineru` 的服务地址与模型 ID 由 `MINERU_VL_SERVER`、`MINERU_VL_MODEL_NAME` 环境变量提供，`--url` 可覆盖服务地址；程序会访问对应的 `/v1/models` 和 `/v1/chat/completions`。

认证优先使用环境变量 `MINERU_VL_API_KEY`，也可用 `--api-key` 覆盖它。避免把密钥直接写进命令行：它可能进入 shell 历史或日志。

```sh
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
export MINERU_VL_API_KEY='<your-key>'

./target/release/mineru -p "input.pdf" -o output
```

若必须临时传入密钥：

```sh
export MINERU_VL_MODEL_NAME="<model-id>"
./target/release/mineru -p "input.pdf" -u "https://<server>/v1" \
  --api-key "<your-key>"
```

---

## `mineru` 规范命令（PDF / 图像 / Office）

`mineru` 是规范产品二进制，支持 PDF、图像和 Office 输入，可选 `--api-url` 远程 API 服务器模式。`--backend=local` 仅在直接模式下通过隔离的内置 Rust `mineru-office-convert` 辅助程序，对 AnyDoc 支持的旧格式和保守判定的干净文本 PDF 执行原生 Markdown 抽取；这是项目私有 native lane，不是本地 ML 模型、`llama-server` 或官方 `hybrid-engine`。该辅助程序不启动 Python、Microsoft Office/LibreOffice，不加载模型，也不发网络请求。直接 `hybrid-http-client` 使用独立的官方 MinerU 4.0.0a6 Python worker，绝不进入旧版 3.4.5 VLM 或 Office 路径。

OOXML 格式转换需要 `mineru-office-convert` 辅助程序；`backend=local` 的旧格式和 native PDF 抽取也使用这个内置 Rust 辅助程序，且不启动 Python、Office、模型或网络。能力依赖两个可选 feature：

```sh
# docx/pptx/xlsx → PDF（经 office2pdf，再走 VLM 版面解析）
cargo build --release --features office
# 旧格式 → 非 local VLM 路径的尽力文本 PDF；local 通过内置 helper 使用 AnyDoc Markdown
cargo build --release --features legacy-office
# 两者都启用
cargo build --release --features office,legacy-office
```

`mineru` 按扩展名自动路由：`.docx`/`.pptx`/`.xlsx` 保持现有 helper 转 PDF 再走 VLM 版面解析。对 `.doc`/`.ppt`/`.xls`/`.odt`/`.rtf`/`.epub`/`.ods`/`.odp`/`.csv`，非 local 直接 VLM 路径先通过隔离 helper 将 AnyDoc Markdown 尽力转换为合法文本 PDF，再把该 PDF 送入现有 PDF/VLM 路径。该结果是仅文本的 fallback，不承诺保持 Office 版式：原版式、图片、表格、公式和宏可能丢失，非 ASCII 字符可能被替换为 `?`；每个文档都会发对应 warning，一个批次的 Office/LibreOffice 建议只发一次。若无法生成合法 PDF，错误会建议先用 Microsoft Office 或 LibreOffice 转成 DOCX/XLSX/PPTX。显式 `--backend local` 时，同一组旧格式和干净 native PDF 都通过隔离的内置 Rust helper 运行 AnyDoc；该路径不启动 Python、Office，不加载模型，也不发网络请求。旧格式输出为 `{输出}/{stem}/office/{stem}.md`，native PDF 输出为 `{输出}/{stem}/native/{stem}.md`，只包含 Markdown，不生成 official JSON 或 assets。扫描、混合、乱码、低质量或不确定 PDF 会明确失败，不回退到 VLM。非 local 旧格式输出位于 `{输出}/{stem}/vlm/`，并保留原始旧格式 origin。直接 Hybrid 在启动 worker 前拒绝 Office 和旧格式；API 路径仍 fail-closed，不使用该 worker。`mineru-api` 的 backend 语义不变，不接受 `local`。

### Office helper containment

辅助程序在转换前对 OOXML 和旧格式签名执行强制预检，并限制输入 32 MiB、输出 64 MiB。旧格式 PDF 是有界的文本 fallback，不承诺保持 Office 版式。

| 平台 | 内存硬限制 | 其他辅助程序限制 |
| --- | --- | --- |
| Linux | `RLIMIT_AS` 1 GiB | CPU 120 秒、`NOFILE` 256、托管 180 秒 wall deadline、进程组清理 |
| Windows | Job Object 1 GiB | CPU 120 秒、托管 180 秒 wall deadline、Job tree 清理 |
| macOS | 无原生硬 RSS 上限 | 强制预检、CPU 120 秒、`NOFILE` 256、托管 180 秒 wall deadline、进程组清理 |

macOS 原生 API 没有可靠且无需 entitlement 的进程 RSS/地址空间硬限制。面向互联网、在原生 macOS 接收不可信 Office 文档的部署，必须使用外部 VM 或容器内存边界提供硬内存隔离。

### 直接 VLM 模式（默认）

不传 `--api-url` 时，`mineru` 直接调用外部 VLM 服务。服务地址和模型由 `MINERU_VL_SERVER`、`MINERU_VL_MODEL_NAME`、`MINERU_VL_API_KEY` 环境变量或 `--url` 覆盖。

### 直接官方 Hybrid 4.0.0a6

直接 `-b hybrid-http-client` 要求 Python 环境中精确安装
`mineru==4.0.0a6`。Rust 二进制内嵌窄 adapter shim，但不打包 Python、MinerU
或模型文件；默认 `per-document` 模式每个文档启动一个新子进程，父进程负责
deadline、取消、管道上限和后代清理。只接受 PDF 和官方图像格式，OOXML/旧 Office
会在启动 worker 前拒绝。

可显式选择 `--official-worker-mode persistent`，或设置
`MINERU_OFFICIAL_WORKER_MODE=persistent`，在同一次直接 CLI 运行中复用一个 worker
和已加载模型。该性能模式始终只有一个 active request：文档仍按顺序、各自使用私有
快照和 bundle；worker 启动/握手一次，取消或崩溃后下一文档建立新 session。已提交的
请求不自动重试，不提供硬 RSS/GPU 隔离。默认值仍为 `per-document`，CLI 显式值覆盖
环境值，环境值只影响直接 `hybrid-http-client`。

`medium` 也保持官方 `hybrid-http-client` backend，但只走本地路径，不需要
VLM URL；`high`/`xhigh` 使用同一个官方 `hybrid-http-client`，必须提供显式 HTTP(S) `--url` 或
`MINERU_VL_SERVER`。`model_stack` 可为 `auto`、`light`、`full`；模型目录和
配置由用户通过绝对路径提供。公式/表格关闭开关不是固定 parser 的参数。
结果独立写入 `{输出}/{stem}/hybrid-v4/`，包含 `markdown.md`、
`middle_json.json`、`content_list.json`、`structured_content.json`、可选的
`model_output.json` 和 `images/`，不会进入 3.4.5 builders。

直接 Hybrid 会拒绝旧版 v3 专用的 VLM transport 控制项（`--http-*`、
`--max-remote-image-*`、`--max-decoded-pixels`、`--max-images-per-request`、
`--max-redirects`、`--vlm-debug`、`--temperature-retry` 及对应环境变量），
也会拒绝 `--client-side-output-generation`，不会静默忽略这些选项。官方解析
字段和项目自己的输入/输出上限仍然可用。

项目自有 adapter envelope 版本为 `mineru-rs-official-worker/1`（默认模式）或
内部 persistent `mineru-rs-official-worker/2`，都不是官方 MinerU stdin/stdout 协议。
API 模式仍明确拒绝 Hybrid：

```text
failed: backend=hybrid-http-client is direct-only; API mode does not support Hybrid
```

默认 `vlm-http-client` 始终走现有 3.4.5 VLM 路径；`backend=local` 和官方 Hybrid 也保持独立。

```sh
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
export MINERU_VL_API_KEY="<your-key>"

./target/release/mineru -p input.pdf -o output
```

### 本地 AnyDoc 模式（`backend=local`）

`local` 表示在隔离的内置 Rust `mineru-office-convert` 辅助程序中执行 AnyDoc 文本抽取，不在 CLI 核心进程中运行，也不表示本地模型。它支持 `.doc`、`.ppt`、`.xls`、`.odt`、`.rtf`、`.epub`、`.ods`、`.odp`、`.csv` 和 PDF native Markdown；构建时必须启用 `legacy-office`：

```sh
cargo build --release --features legacy-office
./target/release/mineru -p old.doc -o output --backend local
```

旧格式输出仍为 `output/old/office/old.md`，干净 native PDF 输出为 `output/old/native/old.md`。native profile 只有 Markdown，不生成 `document.json`、`middle.json`、`content-list` 或 assets。扫描、混合、乱码、空、低质量、复杂或不确定 PDF 会明确报错；不会调用 VLM 或静默回退。该内置 Rust helper 不启动 Python、Microsoft Office/LibreOffice，不加载模型，也不发网络请求。`--url`、`--api-key` 或 VLM 连接环境变量不会用于 AnyDoc 或被校验，也不会访问 `--api-url`。VLM transport flags（如 `--http-*`、`--max-remote-image-*`、`--vlm-debug`）同样不参与 local 解析。local 使用 helper 的有界默认策略，目前仅执行实际可实现的输入/输出字节限制（包括 `--office-input-bytes` 与 `--office-output-bytes`）；helper 专属的 stderr、wall、CPU、NOFILE、内存和进程隔离参数若通过 flag 或环境变量设置，在未明确支持时会在读取输入前明确拒绝。native local PDF 不支持页选择。

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
| `-m, --method <auto\|txt\|ocr>` | `auto` | 解析方法；直接 `vlm-http-client` 忽略，官方直接 Hybrid 转发。 |
| `-b, --backend <vlm-http-client\|hybrid-http-client\|local>` | `vlm-http-client` | 后端。`local` 通过内置 Rust helper 调用项目私有 AnyDoc lane；直接 `hybrid-http-client` 使用官方 4.0.0a6 worker，API Hybrid 仍明确拒绝。 |
| `--effort <medium\|high\|xhigh>` | `medium` | 官方直接 Hybrid 力度。`medium` 仅本地；`high`/`xhigh` 要求显式 HTTP(S) VLM URL；其它直接后端仍只接受 `medium`/`high`。 |
| `--model-stack <auto\|light\|full>` | `auto` | 官方直接 Hybrid 模型栈。显式提供的值（包括 `auto`）覆盖 `MINERU_MODEL_STACK`；省略时使用环境值。模型文件不随 Rust 二进制提供。 |
| `--official-worker-mode <per-document\|persistent>` | `per-document` | 官方 Hybrid worker 生命周期。`persistent` 是单 worker、单 active request 的显式性能模式；覆盖 `MINERU_OFFICIAL_WORKER_MODE`。 |
| `--official-python <绝对路径>` | Python `python3`/`python` | 官方 Hybrid Python 解释器，覆盖 `MINERU_OFFICIAL_PYTHON`；不随 Rust 二进制提供。 |
| `--official-model-dir <绝对路径>` | 无 | 官方 Hybrid 模型根目录，覆盖 `MINERU_MODEL_BASE_DIR`。 |
| `--official-config <绝对路径>` | 无 | 官方 Hybrid 配置路径，覆盖 `MINERU_CONFIG`。 |
| `-l, --lang <语言>` | `ch` | 语言代码。 |
| `-u, --url <URL>` | 无 | 直接模式下的 VLM 服务地址覆盖；API 模式下的任务级模型服务器覆盖。 |
| `-s, --start <n>` | `0` | 起始页，**从 0 开始**。 |
| `-e, --end <n>` | 无（到末页） | 结束页，**包含该页**。 |
| `-f, --formula <true\|false>` | `true` | 公式识别。显式布尔值，优先级 `CLI > MINERU_FORMULA_ENABLE > 默认值`。 |
| `-t, --table <true\|false>` | `true` | 表格识别。显式布尔值，优先级 `CLI > MINERU_TABLE_ENABLE > 默认值`。 |
| `--image-analysis <true\|false>` | `true` | 图像分析。显式布尔值，优先级 `CLI > MINERU_IMAGE_ANALYSIS_ENABLE > 默认值`。 |
| `--log-level <级别>` | `info` | 日志级别：`trace`、`debug`、`info`、`success`、`warning`、`error`、`critical`。覆盖 `MINERU_LOG_LEVEL`。 |
| `--processing-window-size <n>` | `64` | 页处理窗口。覆盖 `MINERU_PROCESSING_WINDOW_SIZE`。 |
| `--page-concurrency <n>` | `64` | 页管线并发上限（任意正整数），仅约束同时运行的页管线数；实际请求级并发由 `--http-max-concurrency`/`MINERU_VLM_HTTP_CONCURRENCY` 决定。覆盖 `MINERU_OFFICIAL_PAGE_CONCURRENCY`。 |
| `--concurrency-model <classic\|two-phase>` | `classic` | 并发模型：`classic` 经典单编码器流水（默认）；`two-phase` 将页内语义处理拆为 encode-all → request-all 两阶段（可选）。覆盖 `MINERU_OFFICIAL_CONCURRENCY_MODEL`。 |
| `--render-workers <n>` | `min(cpu, 8)` | 渲染 worker 数；实际值还受所选页数约束。覆盖 `MINERU_PDF_RENDER_THREADS`。 |
| `--render-timeout-seconds <n>` | `300` | 单次渲染超时。覆盖 `MINERU_PDF_RENDER_TIMEOUT`。 |
| `--batch-size <n>` | `64` | 每页语义推理请求准入（推理批大小），区别于页并发与处理窗口；two-phase 模型下为每页请求阶段并发上限（受全局请求级信号量二次约束）。覆盖 `MINERU_BATCH_SIZE`。 |
| `--total-deadline-seconds <n>` | `86400` | 单文档总 deadline。覆盖 `MINERU_TOTAL_DEADLINE_SECONDS`。 |
| `--max-pdf-bytes <n>` | `1073741824` | 常驻源 PDF 上限。覆盖 `MINERU_MAX_PDF_BYTES`。 |
| `--max-pages <n>` | `10000` | 每文档最大选中页数。覆盖 `MINERU_MAX_PAGES`。 |
| `--max-page-pixels <n>` | `100000000` | 单页像素上限。覆盖 `MINERU_MAX_PAGE_PIXELS`。 |
| `--max-rendered-image-bytes <n>` | `67108864` | 单次渲染 RGB 上限。覆盖 `MINERU_MAX_RENDERED_IMAGE_BYTES`。 |
| `--max-in-flight-image-bytes <n>` | `536870912` | 在途 RGB 预算。覆盖 `MINERU_MAX_IN_FLIGHT_IMAGE_BYTES`。 |
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
| `--http-max-concurrency <n>` | `100` | 全局请求级准入信号量（layout 与语义请求共用），决定服务端在途请求深度；建议 ≤ 服务端 vLLM `max_num_seqs`。覆盖 `MINERU_VLM_HTTP_CONCURRENCY`。 |
| `--http-timeout-seconds <n>` | `600` | `MINERU_VLM_HTTP_TIMEOUT` |
| `--connect-timeout-seconds <n>` | `10` | `MINERU_VLM_CONNECT_TIMEOUT` |
| `--http-max-keepalive-connections <n>` | `20` | `MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS` |
| `--http-keepalive-expiry-seconds <n>` | `30` | `MINERU_VLM_HTTP_KEEPALIVE_EXPIRY` |
| `--http-max-retries <n>` | `3` | `MINERU_VLM_HTTP_MAX_RETRIES` |
| `--http-retry-backoff-factor <f>` | `0.5` | `MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR` |
| `--max-remote-image-bytes <n>` | `33554432` | `MINERU_VLM_MAX_IMAGE_BYTES` |
| `--max-decoded-pixels <n>` | `100000000` | `MINERU_VLM_MAX_DECODED_PIXELS` |
| `--max-images-per-request <n>` | `64` | `MINERU_VLM_MAX_IMAGES_PER_REQUEST` |
| `--max-redirects <n>` | `3` | `MINERU_VLM_MAX_REDIRECTS` |
| `--http-max-response-bytes <n>` | `10485760` | `MINERU_VLM_HTTP_MAX_RESPONSE_BYTES` |
| `--temperature-retry[=<true\|false>]` | 关闭 | 仅对可完整缓冲的 official PDF layout/semantic 请求启用质量重试：先使用基础温度，之后每次 `+0.2`，上限 `1.0`。升温重试 body 仅将已存在的正数 `top_k` 放宽到至少 `40`、`top_p` 放宽到至少 `0.9`；不添加缺失字段或改写 `top_k<=0` 的不限值。`--temperature-retry` 等同于 `true`，显式 `=false` 覆盖 `MINERU_VLM_TEMPERATURE_RETRY`；未提供 CLI 值时沿用环境变量。不影响普通 `predict`、批量、流式、`backend=local` 或 API 表单请求。 |
| `--vlm-debug <true\|false>` | `false` | 在 VLM 请求体中发送 `vllm_xargs.debug`。覆盖 `MINERU_VL_DEBUG_ENABLE`。 |

诊断/人类输出截断上限保持编译固定、不可配置。现有 `--max-input-bytes`、`--max-encoded-document-bytes`、`--max-output-bytes` 三组不变。

现有直接 `vlm-http-client` 下 `--method`、`--effort`、`--lang` 的非默认值会产生警告并被忽略；官方直接 Hybrid 会把这些字段传给 4.0.0a6。`--client-side-output-generation` 在直接 Hybrid 和 API 模式下都会被拒绝。

API 模式下本地 VLM 传输旋钮（`--page-concurrency`、`--concurrency-model`、`--processing-window-size`、`--render-*`、`--batch-size`、全部 `--http-*`/`--max-remote-image-bytes`/`--max-decoded-pixels`/`--max-images-per-request`/`--max-redirects`/`--http-max-response-bytes`/`--temperature-retry`/`--vlm-debug` 及其环境拼写）会显式报错，因为远程服务器执行解析、这些配置不会有任何消费者；`MINERU_VL_SERVER` 在未传 `--url` 时作为任务级 `server_url` 提交。

---

## API 服务端

`mineru-api` 是 HTTP API 服务。服务本身不做本地推理，它接收文档、调用外部 VLM 服务，再把结果归档返回。

### 容器

已发布的 Rust API 镜像为 `ghcr.io/agentsyaml/mineru-cli`。它监听容器端口
`8000`，提供 `GET /health`，将任务输出写入 `/app/output`，并以默认的非
root 用户运行。发布二进制包含 `office,legacy-office` feature，但镜像只打包
Rust 二进制：不包含 Python、`mineru==4.0.0a6` 或模型文件。

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

宿主机绑定的输出目录必须允许镜像默认的非 root 用户写入。该镜像不能直接
运行官方 Hybrid；只有显式提供另行准备好的环境才可满足其外部依赖，API Hybrid
仍 fail-closed。`MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT=true` 是显式的、针对单个容器的
未认证任务 API opt-in；不要将它放入 Dockerfile 或镜像的全局 ENV。优先按上例发布到
loopback；若要扩大暴露范围，必须使用私有网络或带认证的反向代理，因为 API 没有内置
认证或任务所有权隔离。

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
| `MINERU_API_FILE_CAP` | `1073741824` | 单文件上传字节上限。 |
| `MINERU_API_BODY_CAP` | `1074790400` | multipart 请求体字节上限。 |
| `MINERU_API_TEXT_CAP` | `65536` | 单个表单文本字段字节上限。 |
| `MINERU_API_TEXT_TOTAL_CAP` | `262144` | 表单文本合计字节上限。 |
| `MINERU_API_FORM_FIELDS_CAP` | `32` | multipart 表单字段数量上限。 |
| `MINERU_TASK_RESULT_TIMEOUT_SECONDS` | `3600` | `mineru --api-url` 客户端：任务结果超时秒数。 |
| `MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS` | `600` | 客户端：结果下载超时秒数。 |
| `MINERU_API_CONNECT_TIMEOUT_SECONDS` | `10` | 客户端：API 连接超时秒数。 |
| `MINERU_API_ACQUISITION_TIMEOUT_SECONDS` | `60` | 客户端：任务提交/状态获取超时秒数。 |
| `MINERU_API_SEND_TIMEOUT_SECONDS` | `300` | 客户端：上传超时秒数。 |
| `MINERU_API_POLL_INTERVAL_SECONDS` | `1` | 客户端：轮询间隔秒数。 |
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
| `MINERU_OOXML_ARCHIVE_BYTES` | `1073741824` | OOXML 预检归档字节上限。 |
| `MINERU_OOXML_EXPANDED_BYTES` | `268435456` | OOXML 预检解压字节上限。 |
| `MINERU_OOXML_XML_ENTRY_BYTES` | `8388608` | OOXML 单个 XML 条目字节上限。 |
| `MINERU_OOXML_XML_TOTAL_BYTES` | `33554432` | OOXML XML 合计字节上限。 |
| `MINERU_OOXML_RATIO` | `500` | OOXML 条目压缩比上限。 |
| `MINERU_OOXML_XML_DEPTH` | `128` | OOXML XML 深度上限。 |
| `MINERU_OOXML_XML_EVENTS` | `100000` | OOXML XML 事件数上限。 |
| `MINERU_OOXML_XML_ATTRIBUTES` | `256` | OOXML 单元素属性数上限。 |
| `MINERU_OOXML_XML_NAMESPACES` | `256` | OOXML 单元素命名空间数上限。 |
| `MINERU_ARCHIVE_MAX_ENTRIES` | `100000` | 归档最大条目数（ZIP 扫描）。 |
| `MINERU_ARCHIVE_MAX_RATIO` | `1000` | 归档条目压缩比上限。 |
| `MINERU_ZIP_SCAN_CENTRAL_CAP` | `67108864` | ZIP 中央目录扫描字节上限（64 MiB）。 |
| `MINERU_ZIP_SCAN_NAME_CAP` | `4096` | ZIP 单条目名长度上限（字节）。 |
| `MINERU_ZIP_SCAN_DEPTH_CAP` | `64` | ZIP 条目路径深度上限。 |
| `MINERU_ZIP_SCAN_TOTAL_NAME_CAP` | `33554432` | ZIP 条目名合计字节上限（32 MiB）。 |
| `MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP` | `1000000` | ZIP 路径组件合计上限。 |
| `MINERU_PROCESSING_WINDOW_SIZE` | `64` | 页处理窗口。 |
| `MINERU_OFFICIAL_PAGE_CONCURRENCY` | `64` | 页管线并发上限（任意正整数），仅约束同时运行的页管线数；请求级并发由 `MINERU_VLM_HTTP_CONCURRENCY` 决定。 |
| `MINERU_OFFICIAL_CONCURRENCY_MODEL` | `classic` | 并发模型，取值 `classic\|two-phase`。`classic`：经典单编码器流水（默认）；`two-phase`：将页内语义处理拆为 encode-all → request-all 两阶段，解除 CPU 编码对请求派发的串行瓶颈（可选启用）。 |
| `MINERU_MODEL_STACK` | `auto` | 官方直接 Hybrid 模型栈：`auto\|light\|full`。 |
| `MINERU_OFFICIAL_WORKER_MODE` | `per-document` | 官方 Hybrid worker 模式：`per-document\|persistent`。仅直接 Hybrid 生效；CLI 显式值优先。 |
| `MINERU_OFFICIAL_PYTHON` | Python `python3`/`python` | 官方 Hybrid Python 解释器绝对路径。 |
| `MINERU_MODEL_BASE_DIR` | 无 | 官方 Hybrid 模型根目录绝对路径。 |
| `MINERU_CONFIG` | 无 | 官方 Hybrid 配置绝对路径。 |
| `MINERU_PDF_RENDER_THREADS` | `min(cpu, 8)` | 渲染 worker 数。 |
| `MINERU_PDF_RENDER_TIMEOUT` | `300` | 单次渲染超时秒数。 |
| `MINERU_FORMULA_ENABLE` | 开启 | 公式识别默认值（严格 `true`/`false`，不区分大小写）。 |
| `MINERU_TABLE_ENABLE` | 开启 | 表格识别默认值（严格 `true`/`false`）。 |
| `MINERU_IMAGE_ANALYSIS_ENABLE` | 开启 | 图像分析默认值（严格 `true`/`false`）。 |
| `MINERU_LOG_LEVEL` | `info` | 日志级别；`critical` 静默进度输出。 |
| `MINERU_BATCH_SIZE` | `64` | 每页语义推理请求准入（two-phase 下为每页请求阶段并发上限）。 |
| `MINERU_TOTAL_DEADLINE_SECONDS` | `86400` | 单文档总 deadline。 |
| `MINERU_MAX_PDF_BYTES` | `1073741824` | 常驻源 PDF 上限。 |
| `MINERU_MAX_PAGES` | `10000` | 每文档最大选中页数。 |
| `MINERU_MAX_PAGE_PIXELS` | `100000000` | 单页像素上限。 |
| `MINERU_MAX_RENDERED_IMAGE_BYTES` | `67108864` | 单次渲染 RGB 上限。 |
| `MINERU_MAX_IN_FLIGHT_IMAGE_BYTES` | `536870912` | 在途 RGB 预算。 |
| `MINERU_MAX_RAW_OUTPUT_BYTES` | `134217728` | 单文档原始输出预算。 |
| `MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE` | `256` | 单页版面块上限。 |
| `MINERU_MAX_SEMANTIC_REQUESTS_PER_PAGE` | `128` | 单页语义请求上限。 |
| `MINERU_MAX_ENCODED_REQUEST_BYTES` | `16777216` | 编码请求上限。 |
| `MINERU_MAX_ENCODED_BATCH_BYTES` | `67108864` | 编码批上限。 |
| `MINERU_MAX_TOTAL_ASSET_BYTES` | `1073741824` | 全部资产上限。 |
| `MINERU_MAX_STAGED_TEXT_BYTES` | `268435456` | 暂存文本上限。 |
| `MINERU_VLM_HTTP_CONCURRENCY` | `100` | 全局请求级准入信号量（layout 与语义请求共用）；建议 ≤ 服务端 vLLM `max_num_seqs`。 |
| `MINERU_VLM_HTTP_TIMEOUT` | `600` | VLM HTTP 请求超时秒数。 |
| `MINERU_VLM_CONNECT_TIMEOUT` | `10` | 连接超时秒数。 |
| `MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS` | `20` | keepalive 连接池大小。 |
| `MINERU_VLM_HTTP_KEEPALIVE_EXPIRY` | `30` | keepalive 过期秒数。 |
| `MINERU_VLM_HTTP_MAX_RETRIES` | `3` | HTTP 重试次数。 |
| `MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR` | `0.5` | 重试退避因子。 |
| `MINERU_VLM_MAX_IMAGE_BYTES` | `33554432` | 远程图像字节上限。 |
| `MINERU_VLM_MAX_DECODED_PIXELS` | `100000000` | 解码像素上限。 |
| `MINERU_VLM_MAX_IMAGES_PER_REQUEST` | `64` | 每请求图像数上限。 |
| `MINERU_VLM_MAX_REDIRECTS` | `3` | 重定向上限。 |
| `MINERU_VLM_HTTP_MAX_RESPONSE_BYTES` | `10485760` | VLM HTTP 响应上限。 |
| `MINERU_VLM_TEMPERATURE_RETRY` | 关闭 | 取值 `1`/`true` 开启（`0`/`false` 关闭）可完整缓冲的 official PDF layout/semantic 质量重试。先发基础温度，之后每次 `+0.2`，上限 `1.0`；使用独立重试预算，并共享 official deadline 与响应字节预算。 |
| `MINERU_VLM_TEXT_BEFORE_IMAGE` | 关闭 | 请求中文本置于图像之前。 |
| `MINERU_VLM_ALLOW_TRUNCATED_CONTENT` | 关闭 | 允许截断的 VLM 响应内容。 |
| `MINERU_VLM_ALLOW_REMOTE_IMAGES` | 关闭 | 允许按 URL 拉取远程图像。 |
| `MINERU_VLM_ALLOW_PRIVATE_REMOTE_IMAGES` | 关闭 | 允许私有/回环地址的远程图像。 |
| `MINERU_VLM_END_TOKEN` | `<|im_end|>` | VLM 响应的结束 token。 |
| `MINERU_VL_DEBUG_ENABLE` | 关闭 | VLM 请求调试标记（严格 `true`/`false`）。 |
| `MINERU_VL_SERVER` | 无 | VLM 服务基础 URL（如 `https://host/v1`）；`mineru` 直接模式与 `mineru-api` 必填。 |
| `MINERU_VL_MODEL_NAME` | 无 | 模型 ID；`mineru` 直接模式与 `mineru-api` 必填。 |
| `MINERU_VL_API_KEY` | 无 | VLM 服务的 Bearer 令牌。 |

前缀说明：`MINERU_VL_*` 为遗留前缀（VLM 服务连接核心配置：服务地址、模型 ID、API 密钥），新的传输旋钮统一使用 `MINERU_VLM_*` 前缀。

对规范 CLI，每个数值与布尔变量均为严格解析：布尔只接受不区分大小写的 `true`/`false`（`1`、`yes`、`on` 会报错，不再静默视为关闭）；`MINERU_VLM_TEMPERATURE_RETRY` 这个 opt-in 开关额外接受 `0`/`1`。数值的非法、非有限、不应为零却为零、溢出或平台不可表示的值会在任何网络/输出工作前失败，不再回落到默认值。（例外：三个服务端布尔 `MINERU_API_PUBLIC_BIND_EXPOSED` / `MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT` / `MINERU_API_SHUTDOWN_ON_STDIN_EOF` 仍接受 `1`/`true`/`yes`/`on`。）

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

表单接受 `files` 文件部分，以及 `lang_list`、`backend`、`effort`、`parse_method`、`formula_enable`、`table_enable`、`image_analysis`、`start_page_id`、`end_page_id`、`server_url`、`response_format_zip`、`return_md`、`return_middle_json`、`return_model_output`、`return_content_list`、`return_images`、`return_original_file`、`client_side_output_generation` 文本字段。字段重复、字段过多或取值非法都会被拒绝。

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

`{stem}` 是路径输入文件名去掉扩展名后的安全 stem；无安全 stem 时为 `document`。库 API 以字节传入 `PdfInput::Bytes` 且未提供安全 stem 时，预览同为 `document_layout.pdf`。输出先写入同级临时 staging 目录；完成后以重命名替换目标目录，已有目录会先作为备份，替换成功后删除备份，避免留下半写入结果。

直接 CLI 的 native profile 与 official 输出树明确分离：

```text
output/{stem}/native/{stem}.md
```

其中只有 native Markdown，不提供 `document.json`、`middle.json`、
`content-list`、layout preview 或裁剪 assets；不要将其当作 official
MinerU 结果归档。

## 库 API（最小示例）

以下示例只使用公开 API，可放入自己的 Tokio 异步程序：

```rust
use mineru::{RunOptions, run};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run(RunOptions::new("input.pdf", "out/")).await?;
    Ok(())
}
```

认证来自 `MINERU_VL_API_KEY`（服务端点与模型分别用 `MINERU_VL_SERVER`、`MINERU_VL_MODEL_NAME`）。若要在代码中覆盖服务端点、模型或认证，请在调用 `run` 前设置 `RunOptions` 的公开字段（`api_url`、`api_key`）。

## Python 和 Node.js 绑定

`mineru-rs` Python 软件包和 `@alexsun-top/mineru` Node.js 软件包封装同一个解析器。两者都提供 `parse()`（在内存中返回 markdown）和 `run()`（写入完整输出树）。

> 绑定包不打包 `mineru-office-convert` 辅助程序，暂不支持 Office 格式（`.docx`/`.pptx`/`.xlsx`）输入转换；传入 Office 文档会报 "office conversion is unavailable"。PDF 与图像输入不受影响。需要 Office 转换时请使用 `cargo install mineru --features office` 的 CLI 或 `mineru-api` 服务端。

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
    await asyncio.to_thread(
        Path("out.md").write_text, result.markdown, encoding="utf-8"
    )


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

两者接受与绑定版本相同的 VLM 关键字选项；`backend=local` 当前仅属于规范 `mineru` CLI，不改变绑定或 API backend 语义。

### Node.js

```sh
pnpm add @alexsun-top/mineru   # 或：npm install @alexsun-top/mineru
```

```ts
import { writeFile } from 'node:fs/promises'
import mineru from '@alexsun-top/mineru'

const { markdown } = await mineru.parse({ path: 'input.pdf' })
await writeFile('out.md', markdown)
```

`parse()` 解析为 `{ markdown, warnings }`；markdown 字符串在内存中返回，由调用方决定如何持久化。`run()` 写入完整输出树并解析为 `{ warnings }`：

```ts
import mineru from '@alexsun-top/mineru'

const { warnings } = await mineru.run({ path: 'input.pdf', output: 'out/' })
if (warnings.length) console.warn(warnings)
```

选项以 camelCase 命名镜像 CLI：`apiUrl`、`method`、`backend`、`effort`、`lang`、`url`、`start`、`end`、`formula`、`table`、`imageAnalysis` 和 `clientSideOutputGeneration`。

## 默认资源限制

### 文档大小控制

`--max-input-bytes` / `MINERU_MAX_INPUT_BYTES`、`--max-encoded-document-bytes` / `MINERU_MAX_ENCODED_DOCUMENT_BYTES` 和 `--max-output-bytes` / `MINERU_MAX_OUTPUT_BYTES` 接受无符号十进制字节数（允许空白和 `_`）。优先级为 CLI、环境变量、编译默认值：输入 4_293_918_719 字节、编码文档 8 GiB、输出 8 GiB。显式的非法、零、溢出或平台不可表示值会失败；不再存在任意硬上限——配置值本身作为策略使用，而不会被夹紧到另一个常数。

这些是磁盘/文档总量而非常驻内存分配：解析后的 PDF 和当前 PDF 压缩器会在 `lopdf` 加载前拒绝超过常驻上限（`--max-pdf-bytes` / `MINERU_MAX_PDF_BYTES`，默认 1 GiB）的源 PDF，单个 VLM 响应仍限制为 10 MiB（`--http-max-response-bytes`）。编码策略应在 `mineru-api` 配置；规范远程模式会拒绝编码覆盖项。

| 项目 | 默认值 |
| --- | ---: |
| PDF 大小 / 页数 | 1 GiB / 10,000 页 |
| 单页像素 / 渲染 RGB 图像 | 100,000,000 / 64 MiB |
| 响应体 / 全部资产 | 10 MiB / 1 GiB |
| 单页版面块数 / 页窗口 | 256 / 64 页 |
| 单页语义请求数 / 推理批 | 128 / 64 |
| 同时在途渲染图像 | 512 MiB |
| 请求并发 / 渲染 worker | 100 / min(cpu, 8)（覆盖值仍受 CPU 与所选页数约束） |
| 官方页准入并发 | 64（无固定上限） |
| 连接 / 单请求 / 总解析超时 | 10 秒 / 600 秒 / 24 小时 |

内存占用随在途图像预算缩放：A4 文档在默认 512 MiB 预算下实测约 2.4-2.5 GB RSS，主要由常驻解析后的 PDF 与每窗口渲染 RGB 构成（文档越大越接近上限）。提高预算以内存换速度：1 GiB 约需 4-5 GB RSS，仅换来大文档约 10% 的墙钟时间收益。API 服务模式下每个并发任务都携带该预算，`MINERU_API_MAX_CONCURRENT_REQUESTS`（默认 3）会成倍放大内存占用；内存受限主机请调低在途预算（`MINERU_MAX_IN_FLIGHT_IMAGE_BYTES` / `--max-in-flight-image-bytes`）。

### 默认值来源与容量

- **上游锁定**：200 DPI、64 页窗口、VLM HTTP 最大并发 100、HTTP 请求超时 600 秒。渲染 worker 不再上游锁定，默认值为 min(cpu, 8)。
- **Rust 防护**：10 秒连接超时、24 小时总超时，以及页数、PDF、资产、响应、渲染图像、像素、在途图像和版面块限制。

10,000 页支持仅是高内存下的尽力而为：输入字节、最终页面结果和资产都会保留在内存中，并非无上限保证。通过环境变量（`MINERU_MAX_*`、`MINERU_VLM_*` 等）与 CLI 参数（`--page-concurrency`、`--render-workers`、`--total-deadline-seconds` 等）按可用 RAM 和服务端点容量配置。所有限制、并发和 worker 必须大于零；所有超时必须非零，且单请求超时不得超过总超时。

## 输入上限与放大配置

流水线在多个独立阶段执行大小上限。触发上限时，报错消息会给出具体文件名、大小、限制值与放大旋钮（flag 或环境变量）；单个文档失败不会中断整批处理，其余文档继续。本地解析大文件会按文件大小占用内存（上面的磁盘/文档总量与下面的常驻上限相互独立）。

| 上限 | 默认值 | Flag | 环境变量 | 触发阶段 |
| --- | ---: | --- | --- | --- |
| 本地驻留/解析上限 `max_pdf_bytes` | 1 GiB | `--max-pdf-bytes` | `MINERU_MAX_PDF_BYTES` | 文件读取与 PDF 本地解析（含办公室文档转换后 PDF） |
| 输入传输上限 `max_input_bytes` | 4_293_918_719（≈4 GiB） | `--max-input-bytes` | `MINERU_MAX_INPUT_BYTES` | 输入摄取/传输 |
| 输出上限 `max_output_bytes` | 8 GiB | `--max-output-bytes` | `MINERU_MAX_OUTPUT_BYTES` | 输出生成 |
| OOXML 归档上限 | 1 GiB | `--ooxml-archive-bytes` | `MINERU_OOXML_ARCHIVE_BYTES` | Office 文档预检 |
| Office 转换输入上限 | 32 MiB | `--office-input-bytes` | `MINERU_OFFICE_INPUT_BYTES` | Office 转换 |
| 服务器端文件上限（`--api-url` 模式） | 1 GiB | `--file-cap`（服务端 `mineru-api`） | `MINERU_API_FILE_CAP`（服务端） | 服务器上传 |

## 限制与排错

- Hayro 不支持加密 PDF；复杂/高级 PDF 效果的渲染可能与其他渲染器不同。遇到无效 PDF、页映射不一致、尺寸限制或渲染异常会明确失败，不会静默跳过。
- 预览支持页面旋转 `0/90/180/270`。其目标是可用的视觉与语义对齐；由于写入了标注且 PDF 序列化会变化，预览文件字节不等于原 PDF。其他旋转会失败。
- `401` 通常是缺失或无效的 API key；`404` 通常是 `--url` / `MINERU_VL_SERVER` 路径不对。确认服务实际暴露 `/v1/models` 与 `/v1/chat/completions`。
- 模型校验失败时（`GET /v1/models` 未返回所配置的模型，或未配置模型但端点返回多个模型），确认 `GET /v1/models` 返回的 `data` 中含所选 ID，并检查认证和 base URL。
- `no valid layout tokens` 表示服务返回内容不含 MinerU 所需的版面 token；请选择兼容的 MinerU VLM 模型/服务，而不是普通聊天模型。
- `limit exceeded` 表示超过上表资源上限；缩小输入或在库调用中调整并验证配置。Hayro 不支持的 PDF 则需用支持该 PDF 特性的文件/渲染流程处理后再试。
