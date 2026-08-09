# MinerU mistral.rs CLI TODO

- [x] 创建隔离 worktree，并迁移可行性调研报告
- [x] 添加 `mistralrs` CPU feature、`mistralrs-cuda` feature 和独立 CLI
- [x] 实现模型目录配置、显式自动下载与输入安全校验
- [x] 接入 Qwen2-VL 模型 builder、请求适配和 MinerU 官方 PDF 流程
- [x] 完成 Oracle gate 1 与架构修复
- [x] 完成 Oracle gate 2 调研，定位 special-token 解码阻塞
- [x] 固定并补丁 mistralrs-core 0.8.1，使生成结果保留 MinerU 协议 token
- [x] 添加无需权重的 special-token 回归测试
- [x] 修复 macOS CPU 可用内存探测为 0 时的 mistral.rs auto-map 失败
- [x] 修复 mistral.rs 0.8.1 Qwen2-VL 首个 decode token 的 mrope 索引越界
- [x] 完成真实模型 smoke：整页 layout 回复解析出 block，bbox 合法，两次 greedy 完全一致
- [x] 关闭 Oracle gate 2 的全部阻塞项

### 真实模型 smoke 状态（mrope 修复后，Gate 2 通过）

- 已用已下载的固定 revision snapshot 执行 `real_model_layout_reply_preserves_special_tokens`：
  - mrope 越界崩溃已消失（首个 decode 不再 index-select OOB），生成完整且与图像内容一致。
  - 两张 fixture 图均为 PDF 内嵌内容裁片（937x37 公式/流程图条带、801x509 对比表格图），模型按内容正确输出
    LaTeX 公式 / `<fcel>` 表格协议 token（证明 special-token 补丁端到端生效），但均未进入 layout-block 模式，
    原始回复不含 `<|box_start|>`，断言失败。
  - 新根因（第一次）：layout 检测需要整页渲染图（MinerU 官方对整页调用 layout 模型），现有 fixture 仅内嵌裁片；
    属于测试图/门禁匹配问题，不是 mrope 补丁的 runtime bug。
- 第二次根因（整页图 + 原始 JPEG）：`prepare_for_layout` 之前直接喂 1200×1600 原始 JPEG，模型返回表格协议
  （单个 `<|box_start|>` + `<fcel>` 单元格，无坐标/`<|box_end|>`），生产 parser 拒绝。修正为 smoke 先走生产
  `prepare_for_layout`（1036×1036 PNG）再推理，行为与生产 layout 流程一致。
- 最终 release-mode smoke 通过（52.41s，含 release 编译 2:31 总计）：
  - raw layout 回复含 `<|box_start|>`，示例 token：`<|box_start|>169 342 832 659<|box_end|><|ref_start|>table<|ref_end|><|rotate_up|>`。
  - 生产 `parse_layout_output` 解析出 block（表格 block），每个 bbox 为合法规范坐标（`NormalizedBbox` 校验通过）。
  - 同一已加载 client + 同一图 + greedy sampling 第二次预测 raw 回复字节完全一致，且第二次同样解析非空。
  - 全程 `HF_HUB_OFFLINE=1`，仅使用本地 snapshot，未下载权重。
- [x] 完成 CPU/CUDA feature 构建矩阵
- [x] 添加 CPU Docker 镜像和 CUDA Docker 镜像
- [x] 调整 CI：通用 CPU 检查与 CUDA 专用构建分离
- [x] 完全重写 `README.md`，提供英文用户上手指南
- [x] 完全重写 `README.zh-CN.md`，提供中文用户上手指南
- [x] 更新 CLI/环境变量/Docker 契约测试
- [x] 运行格式化、默认功能、CPU feature、CLI、HTTP 与 official route 验证
- [x] 在可用环境中验证 CUDA 构建；不可用时记录明确证据边界
- [x] 完成 Oracle gate 3 与最终修复
- [x] 对比 base diff，确认主 checkout 未被开发改动污染
