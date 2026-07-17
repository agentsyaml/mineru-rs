# MinerU 3.4.4 功能完整性审计

## 执行摘要

**结论：当前实现没有完整实现官方 MinerU 3.4.4，也不是可替换官方 `mineru` 的完整 drop-in。**

当前真正可用的核心，是一个经过较强边界保护的 Rust 版 **PDF + 远程
OpenAI-compatible VLM (`vlm-http-client`) 子集**：它实现了分页渲染、窗口化
VLM 请求、官方形态的主要产物、事务化输出，以及显式启动的 loopback
protocol-v2 异步任务服务。这个子集是实质性的，不是空壳。

但官方默认路径是 `hybrid-engine`，并要求五类后端、PDF/图片/Office 输入、
`--api-url` 与无 API 时的临时服务启动、完整 `mineru-api`、本地模型运行时及
多种输出模式。当前这些能力大多是“仅有协议/规划代码”或完全缺失，尚未进入
用户可用产品路径。项目现有 README 与兼容性文档没有宣称完整兼容，范围声明
诚实；但它们尚未记录 P3b 已完成的专用任务服务，因此略低估当前能力。

## 审计范围与方法

- 官方基线：MinerU `3.4.4`，tag `mineru-3.4.4-released`，commit
  `0dfc9460cd9ab693b9af60ae3fbffd7bc111b062`。
- 事实优先级：官方源码 > 官方文档 > 本地冻结 fixture > 测试名称或模块名称。
- 本地基线：当前未提交工作树，包含已完成并经 Oracle 审核的 P3b。
- “完整实现”分两层判断：
  1. **核心 `mineru` 等价**：CLI、五后端、五类输入、API 编排和核心输出。
  2. **完整官方产品等价**：再包括 Gradio、router、模型下载及 VLM server 工具。
- 不要求 PDF/图片/JSON 字节完全一致；项目声明的是语义与结构比较。

状态定义：

- **FULL**：当前用户路径可直接使用，且覆盖该项官方契约。
- **SUBSET**：能力真实可用，但只覆盖官方功能的一部分。
- **PLUMBING-ONLY**：已有类型、协议、规划或测试代码，尚未接入可用产品路径。
- **MISSING**：没有对应运行能力。
- **STRONGER-SAFETY-DIVERGENCE**：安全策略更严格，但行为不完全等同官方。

## 总体能力矩阵

| 维度 | 当前状态 | 结论 |
|---|---|---|
| Canonical `mineru` | **SUBSET** | 当前 `src/bin/mineru.rs` 只执行直接 PDF `vlm-http-client` 路径，不是官方默认 `hybrid-engine` |
| `pipeline` | **PLUMBING-ONLY / MISSING ENGINE** | 可在私有 API domain 中表示和规划，但没有官方本地 OCR/layout/table/formula 引擎 |
| `vlm-engine` | **PLUMBING-ONLY / MISSING ENGINE** | 没有 transformers/MLX/vLLM/LMDeploy 本地模型运行时 |
| `vlm-http-client` | **SUBSET** | PDF 远程 VLM 路径、主要产物和显式 loopback 任务服务已实质实现 |
| `hybrid-engine` | **MISSING** | 官方默认后端缺失；不能以 VLM HTTP 替代 |
| `hybrid-http-client` | **PLUMBING-ONLY** | 可序列化给外部官方 API，Rust 自身不执行 hybrid 分析 |
| PDF | **SUBSET** | 可端到端处理；Hayro 对加密及部分高级 PDF 特性有明确限制 |
| 图片输入 | **PLUMBING-ONLY** | 可发现/分类，存在低层图像请求类型，但没有图片到官方文档的产品路径 |
| DOCX/PPTX/XLSX | **PLUMBING-ONLY** | 可做 OOXML 识别和远程任务规划，不能在 Rust 中转换/解析 |
| 远程官方 task API client | **PLUMBING-ONLY** | health/submit/poll/download/安全解压已实现，但模块私有、feature-gated，未接入任何 bin |
| 本地任务 API | **SUBSET** | `mineru-vlm-api` 实现 `/health`、`/tasks`、status/result，仅支持 loopback 单 PDF VLM 异步 ZIP |
| 自动临时 API | **MISSING** | 无 free-port 启动、readiness、stdin EOF、进程树停止与 canonical 自动生命周期 |
| 通用 `mineru-api` | **MISSING** | 无 `/file_parse`、公开绑定、JSON 模式、多文件/多后端及通用参数行为 |
| 官方形态 VLM 产物 | **SUBSET** | Markdown、middle/model、两种 content list、images、layout；P3b ZIP 含 compact origin |
| 选择性产物/客户端生成 | **MISSING** | 不支持完整 return flags、非 ZIP JSON 和 `client_side_output_generation=true` |
| 本地模型、设备与模型源 | **MISSING** | 无模型下载、缓存、device/runtime 选择、预加载或离线推理 |
| Gradio/router/VLM servers | **MISSING** | 官方 ancillary 产品工具未实现 |

## 阻断“完整实现”结论的发现

### 1. CRITICAL：官方默认执行语义不存在

官方 `mineru` 默认 `backend=hybrid-engine`、`method=auto`、`lang=ch`；未指定
`--api-url` 时会启动临时本地 API，再走任务协议。当前
`src/bin/mineru.rs::Cli/run` 只允许 `vlm-http-client` 并直接调用远程模型，既没有
`--api-url`，也不会启动临时 API。

这不是参数名称的小差异，而是默认计算图完全不同。因此当前命令不能作为官方
`mineru` 的默认替代品。

官方锚点：

- `mineru/cli/client.py:1037-1220`
- `mineru/cli/api_client.py:355-635,729-1065`
- 本地冻结契约：`tests/fixtures/official/mineru_3.4.4_cli_contract.json`

### 2. CRITICAL：五个公开后端中只有一个子集能实际执行

当前 `src/mineru_api/mod.rs` 与 `planning.rs` 能表示五种后端，不能据此认定五种
后端已实现：

- `pipeline`：缺少本地 OCR、文本检测/识别、公式与表格模型。
- `vlm-engine`：缺少本地模型和 vLLM/LMDeploy/MLX/transformers 运行时。
- `hybrid-engine`：缺少本地 pipeline + VLM 混合链路。
- `hybrid-http-client`：只能作为远程官方 API 的字段，Rust 不执行 hybrid。
- `vlm-http-client`：唯一真实处理后端，但只覆盖 PDF/远程模型子集。

`src/pipeline.rs` 的名称不能作为官方 pipeline 已实现的证据；它仍通过外部
OpenAI-compatible VLM 做提取。

### 3. HIGH：非 PDF 输入只有发现/协议能力，没有处理引擎

官方支持 PDF、图片、DOCX、PPTX、XLSX。当前
`src/mineru_api/discovery.rs`、`ooxml.rs` 能分类和安全检查输入，也能把它们规划给
外部官方 API；但当前 CLI 与本地服务只真正处理 PDF。

因此“支持 Office/图片”的准确说法只能是“具备内部远程 API 发现与上传前置
能力”，不能说本项目已实现这些格式的解析。

### 4. HIGH：API 是专用子集，不是完整 `mineru-api`

P3b 已实现的 `mineru-vlm-api` 不应被遗漏：

- 有 protocol-v2 `/health`；
- 有 `/tasks`、`/tasks/{id}`、`/tasks/{id}/result`；
- 有 pending/processing/completed/failed 生命周期、容量/保留/清理；
- 有 compact origin PDF、原子 ZIP、重复下载和安全解压组合测试。

但它有意限制为 loopback、恰好一个 PDF、`vlm-http-client`、异步 ZIP；缺少官方
同步 `/file_parse`、公开绑定策略、JSON 返回、多文件/多后端、选择性产物和
完整服务参数。`src/bin/mineru-vlm-api.rs` 也没有 canonical launcher 的 stdin
EOF/信号/子进程托管接线。

### 5. HIGH：内部远程 API 客户端尚不能算用户功能

`src/mineru_api/http.rs`、`runner.rs`、`planning.rs` 已实现相当完整的 protocol-v2
客户端、任务规划、轮询、下载和强化 ZIP 解压；P3b 还用真实私有客户端完成了
服务组合测试。

但该模块仍是私有、feature-gated，当前没有 bin 调用它。代码存在与测试通过只
能证明 P4/P5 的基础设施可用，不能证明 `mineru --api-url` 已经可用。

### 6. HIGH：本地模型生态与官方工具链缺失

官方产品包含模型下载、模型源/配置持久化、device/runtime 选择、本地模型预载，
以及 `mineru-gradio`、`mineru-router`、vLLM/LMDeploy/OpenAI server 命令。当前
项目明确不下载或运行本地模型，也没有这些入口。

这些 ancillary 工具未必阻断“窄范围 CLI 子集”声明，但一定阻断“完整实现官方
功能”的广义声明。

### 7. MEDIUM：输出是结构性子集，不是所有官方模式

直接 VLM 路径的主要输出形态已经较完整，不能简单归类为“没有官方输出”。但
仍有以下缺口：

- 不支持官方完整 return flags 组合；
- 不支持非 ZIP JSON 结果；
- 不支持 `client_side_output_generation=true`；
- 没有完整 span/debug/UI 可视化族；
- Office/pipeline/hybrid 专属目录和产物没有实际生产引擎。

项目的事务发布、no-follow、ZIP 预扫描和资源上限在多处比官方更严格。这属于
**STRONGER-SAFETY-DIVERGENCE**，是优点，但不能用来替代功能完整性证据。

### 8. MEDIUM：CLI 细节仍不等价

当前 canonical-looking CLI 还缺少或偏离：

- `--api-url`、`--method`、`--effort`、`--lang`；
- 官方 `-v/--version`；
- 只转发给临时 API 的未知参数机制；
- 官方一层目录发现与 collision-safe `_N` stem 去重；
- 真实并发 task planning。

当前直接 CLI 的递归 PDF 枚举、重复 stem 报错和顺序 batch 分组是可用设计，但
不等同于官方 3.4.4 行为。

## 当前真正实现好的部分

下列能力有生产代码和可执行测试支撑，应当被明确肯定，而不是与缺失项混在一起：

1. **远程 VLM PDF 路径**：`VlmHttpClient -> MinerUVlmClient -> official_route ->
   official_builders -> official_output` 是真实主链路。
2. **窗口与资源边界**：顺序 page window、独立 HTTP 并发、raw/encoded/RGB/asset
   预算、render timeout 和事务回滚均有覆盖。
3. **主要官方形态产物**：VLM Markdown、middle/model JSON、content list v1/v2、
   images、layout preview；任务 ZIP 另含选页 compact origin。
4. **专用本地任务服务**：loopback-only protocol-v2 异步服务、容量控制、任务
   状态、保留/清理、失败清理、重复结果流和 drain-only shutdown。
5. **强化安全边界**：multipart 流式上限、PDF parent-cycle 拒绝、ZIP64 预扫描、
   路径/链接/冲突防护、原子发布和错误脱敏。
6. **私有远程 API 基础设施**：发现、规划、health、submit/poll/download 和强化
   解压已经具备，且与本地服务做过真实组合测试。

因此最准确的评价不是“什么都没做完”，而是：**一个工程质量较高、边界明确的
官方远程 VLM PDF 子集已经完成，但官方完整产品还没有完成。**

## 测试证据的边界

| 测试 | 能证明什么 | 不能证明什么 |
|---|---|---|
| `tests/official_cli_contract.rs` | fixture 确实冻结了官方事实 | 当前 CLI 已实现 fixture 中的全部事实 |
| `tests/official_route.rs` | PDF 窗口、顺序、预算、回滚和主要产物 | 本地 OCR、Office、hybrid、真实模型质量 |
| `tests/mineru_cli.rs` | 当前受限 CLI 的行为稳定 | canonical MinerU CLI 等价 |
| `src/mineru_api/**` 单测 | 协议、规划、下载、解压基础设施正确 | 这些能力已接入用户命令 |
| P3b 客户端组合测试 | 私有 client 与专用服务真实可组合 | 通用 `mineru-api` 或 canonical auto-launch 已完成 |
| `tests/real_mineru_344_vlm.rs` | 在人工配置下可做结构语义比较 | 默认 CI、全语料、全后端或字节等价 |

当前 active 与 Rust 1.88 全量测试通过，证明实现稳定且 MSRV 可用；它不改变功能
范围。两项真实官方 reference 测试仍是 opt-in ignored，因此也不能把普通绿色 CI
解读为“已经与官方全面等价”。

## P4/P5 与更广义缺口

### 已在当前路线中明确计划

- **P4**：真实 `hybrid-engine` 默认语义、临时本地 API、startup/readiness、stdin
  EOF/进程树 shutdown、output root、retention/cleanup、model source/device 等。
- **P5**：把 `--api-url` 接到私有 P2 client，并在本地默认与远程 API 两条端到端
  路径同时通过后，原子切换 canonical `mineru` surface。

在 P4/P5 完成之前，项目自己的计划也明确禁止宣称 drop-in 完整兼容。

### 当前路线没有证明会覆盖的完整产品能力

- 完整本地 `pipeline` 与 `vlm-engine`；
- 完整图片/Office 本地处理；
- 通用公开 `mineru-api` 的全部端点与模式；
- `mineru-models-download` 与配置持久化；
- `mineru-gradio`、`mineru-router`；
- vLLM/LMDeploy/OpenAI server 命令和部署能力。

即使 P4/P5 完成，也必须重新确认这些 broader product 项是否被纳入范围，才能使用
“完整实现官方功能”的广义表述。

## 文档声明审查

现有文档没有夸大完成度：

- `README.md:5-7` 明确写明只覆盖 MinerU 3.4.4 `vlm-http-client` transport
  baseline，且“不是 general MinerU compatibility claim”。
- `docs/compatibility.md:22-28` 明确写明 **NOT FULL DROP-IN**，并排除本地推理、
  通用 `mineru-api` 和非 PDF 输入。
- `docs/usage.md` 同样限定为 `vlm-http-client` PDF 流程。

但文档已有轻微滞后：

- “不包含 `mineru-api`”对通用官方命令仍然正确，但没有区分现已存在的专用
  `mineru-vlm-api` 单 PDF protocol-v2 服务。
- README 的 “client skeleton” 已低估当前 VLM route、任务生命周期、compact
  origin ZIP 和安全边界的成熟度。

需要避免的只是把 `official_cli_contract`、`mineru_api` 模块名称或全量测试通过，
误读为官方全部功能已完成。

## 最小且真实的当前声明

建议当前只使用以下表述：

> 本项目实现了面向 MinerU 3.4.4 `vlm-http-client` 的 Rust PDF 解析子集，提供
> OpenAI-compatible 远程 VLM 调用、官方形态的主要输出、强化的资源/文件安全
> 边界，以及显式 loopback protocol-v2 单 PDF 异步任务服务。它不包含官方默认
> `hybrid-engine`、本地 pipeline/VLM 引擎、完整 `mineru-api`、非 PDF 本地解析，
> 也不是 MinerU 3.4.4 的完整 drop-in。

## 关键洞察

1. **最大风险不是代码缺少，而是把 plumbing 当产品。** 当前很多官方契约已经被
   精确建模并测试，但没有 bin 调用；完整性审计必须以用户可达路径为准。
2. **P3b 的意义是消除“完全没有本地服务”的说法，不是完成 `mineru-api`。** 它是
   一个边界清晰的专用 VLM 服务，不能被命名相似性扩大解释。
3. **官方默认后端决定了完整性门槛。** 只要 `hybrid-engine` 与自动临时 API 不存在，
   即使远程 VLM 子集质量很高，canonical `mineru` 仍然不等价。
4. **安全增强与兼容性是两个坐标。** 当前 Rust 的文件/ZIP/资源边界多处更强，但
   “更安全”不能自动推导出“功能更完整”。

## 参考资料

### 本地实现

- `README.md`
- `docs/compatibility.md`
- `src/bin/mineru.rs`
- `src/bin/mineru-vlm-api.rs`
- `src/vlm_api.rs`
- `src/mineru_api/`
- `src/official_route.rs`
- `src/official_builders.rs`
- `src/official_output.rs`
- `tests/fixtures/official/mineru_3.4.4_cli_contract.json`
- `tests/official_cli_contract.rs`

### 官方源码与文档

- `mineru/cli/client.py`
- `mineru/cli/api_client.py`
- `mineru/cli/fast_api.py`
- `mineru/cli/common.py`
- `mineru/cli/backend_options.py`
- `mineru/cli/output_paths.py`
- `mineru/utils/ocr_language.py`
- [Quick Start](https://opendatalab.github.io/MinerU/quick_start/)
- [CLI Tools](https://opendatalab.github.io/MinerU/usage/cli_tools/)
- [Quick Usage](https://opendatalab.github.io/MinerU/usage/quick_usage/)
- [Output Files](https://opendatalab.github.io/MinerU/reference/output_files/)
- [Model Source](https://opendatalab.github.io/MinerU/usage/model_source/)
