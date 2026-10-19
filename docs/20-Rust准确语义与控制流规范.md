# 20-Rust 准确语义与控制流规范（RS-01 ～ RS-06）

本文档定义 Symweft 针对 Rust / Cargo 工作空间的构建变体身份、rust-analyzer / SCIP 符号与出现位置适配、Trait 与泛型分派、宏锚点与未保存分析、源码级控制流图（CFG）及资源与工具链回归规范。

## 1. Cargo 工作空间与构建变体（RS-01）

### 1.1 可追踪工作空间与变体身份（`CargoWorkspaceBuildVariant`）
- **五维变体指纹（`variant_fingerprint()`）**：将 `crate graph`（各 crate 源码根、目标类型 `lib`/`bin`/`proc-macro`/`test`、依赖拓扑）、启用 `features` 集合、`target_triple`（如 `aarch64-apple-darwin` / `x86_64-unknown-linux-gnu`）、`edition`（`2015` / `2018` / `2021` / `2024`）、`toolchain_version`（`rustc` 版本与 channel）以及 `Cargo.lock` 摘要和 `execution_policy` 统一纳入确定性 SHA-256 指纹。
- 任一 feature 开关切换、target triple 变更或工具链升级均产生可区分的变体身份，杜绝不同条件编译（`#[cfg(...)]`）结果混用。

### 1.2 `build.rs` 与 `proc-macro` 执行策略及禁用缺口（`DisabledExecutionGapReport`）
- **默认安全策略（`DisabledStaticOnly`）**：默认绝不隐式编译或运行用户工作空间中的 `build.rs` 脚本或 `proc-macro` 动态库（`executes_build_scripts_or_proc_macros = false`）。
- **显式缺口结构化披露**：当处于禁用模式时，`evaluate_execution_policy_gaps()` 明确列出：
  - `crates_with_skipped_build_scripts`：包含未执行 `build.rs` 的 crate 列表；
  - `crates_with_skipped_proc_macros`：未加载的 `proc-macro` crate 列表；
  - `unexpanded_derive_and_attr_macros`：未展开的 `#[derive(...)]` 与属性宏调用；
  - `missing_out_dir_includes`：依赖 `env!("OUT_DIR")` 生成源码的 `include!` 缺口；
  - 并在 `pubcompleteness_note` 中如实声明缺口数量，绝不将未展开宏或缺失生成文件的分析结果伪装成完整编译语义。

## 2. 符号与出现位置导入（RS-02）

### 2.1 固定适配路线与身份规范（`RustOccurrenceImportBatch`）
- **适配路线冻结**：采用 `RustSemanticAdapterRoute::RustAnalyzerScipAdapter`（基于 `rust-analyzer` / SCIP 结构化符号与出现位置协议，提供器标识为 `RUST_PROVIDER_KEY = "a2-rust-scip-v1"`），避免直接耦合不稳定的 `rustc` 内部私有 ABI。
- **跨 Crate 稳定符号身份**：所有 SCIP 描述符统一规范化为 `rs-usr:<descriptor>`（包含包名、版本号、模块路径与类型/函数签名描述符）。
- **多目标重叠去重**：当同一源文件同时被 `lib`、`bin` 或 `test` 目标覆盖时，按 `(scip_symbol, file_path, start_byte, end_byte, role)` 严格去重，并绑定 `variant_fingerprint`。

### 2.2 出现位置索引与完整调用图的诚实边界
- 纯符号/出现位置导入仅提供定义（`Definition`/`Declaration`）与引用（`Reference`/`TypeReference`/`WriteAccess`），缺乏调用者函数归属与多态分派信息。
- `claims_complete_call_graph` 恒为 `false`（由 `validate_import_honesty()` 强制验证），且 `into_occurrence_snapshot()` 生成的 `calls` 为空，绝不将符号出现位置索引冒充为完整调用图（完整调用与 trait 分派由 RS-03 专门构建）。
