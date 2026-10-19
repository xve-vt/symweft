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

## 3. 调用、Trait 与实现绑定（RS-03）

### 3.1 五类分派模型与双向实现跳转（`RustTraitAndCallResolver`）
- **固有方法与自由函数（`InherentOrFreeDirect`）**：直接生成 `dispatch = "direct"` 调用边。
- **静态已知具体类型的 Trait 调用（`StaticTraitImplResolved`）**：精确绑定到具体 `impl Trait for Type` 的方法实现，同时通过 `jump_to_trait_declaration()` 保留回到 Trait 声明的映射。
- **泛型参数约束调用（`GenericTraitBoundPrimary`）**：在 `fn run<T: Storage>(s: &T)` 内调用 `s.flush()` 时，直接绑定至 `Storage::flush` 主声明。
- **`dyn Trait` 动态分派候选（`DynTraitCandidates`）**：对 `&dyn Storage` / `Box<dyn Storage>` 调用生成 `dispatch = "virtual_candidate"` 的候选边集合（覆盖工作空间内全部已知 `impl Storage for ...`），并附带 `open_world_note` 声明外部未索引 crate 可能新增实现。
- **关联项与双向跳转**：完整支持 `Method`、`AssociatedFunction`、`AssociatedType`（如 `type Error = ...`）与 `AssociatedConst` 的声明 $\leftrightarrow$ 实现双向导航。

### 3.2 防单态化爆炸上限（`MAX_RUST_MONOMORPHIZED_INSTANCES_PER_CRATE = 512`）
- 严禁无界物化泛型单态化组合实例；当特化实例数达到单 crate 预算上限（512）时，`record_or_collapse_generic_instance()` 自动折叠回主泛型定义身份并记录 `collapsed_to_primary_generic_count`。

## 4. 宏锚点与未保存分析（RS-04）

### 4.1 声明宏与过程宏双位置锚点（`RustMacroDualAnchor`）
- 覆盖 `macro_rules!`（`DeclarativeMacroRules`）、`#[derive(...)]`（`DeriveProcMacro`）、属性过程宏（`AttributeProcMacro`）与函数式过程宏（`FunctionLikeProcMacro`）。
- 每个由宏展开生成的符号、引用或调用均同时绑定用户源码调用点（`invocation_site`）与宏定义/展开位置（`definition_or_expansion_site`），通过 `to_shared_location()` 映射至共用 `Location { expansion: Some(StageLocation) }`。

### 4.2 多 Dirty 文档协同、策略切换失效与失败不回退（`RustUnsavedOverlayCoordinator`）
- **跨 Crate 级联失效**：`apply_dirty_documents()` 同时接收多个未保存文档修订，自动失效所属 crate 及 `reverse_crate_deps` 中的下游依赖 crate，同时保留未受影响的干净 crate 快照。
- **配置与 `proc-macro` 策略切换**：`on_config_or_policy_change()` 在 `variant_fingerprint` 或 `BuildScriptAndProcMacroPolicy` 变化时全量失效旧快照。
- **失败绝不回退为伪准确结果**：当未保存代码或宏展开失败时，`record_crate_failure()` 立即移除旧快照并记录 `old_disk_facts_masked = true`，绝不回退读取过期的磁盘旧边或将不完整的语法猜测冒充为准确语义。

## 5. Rust 源码控制流图（CFG）（RS-05）

### 5.1 公开源码级控制流构造（`RustFunctionCfg`）
- **节点类型（`RustCfgNodeKind`）**：覆盖 `Entry`、`BasicBlock`、`MatchScrutinee`、`MatchArm`、`LoopHeader`（`loop` / `while` / `for`）、`TryQuestionOperator`（`?` 提前错误返回）、`EarlyReturn`（`return`）、`ClosureCreationSite`、`AsyncAwaitSuspendPoint`（`.await` 挂起点）与 `NormalExit`。
- **边类型（`RustCfgEdgeKind`）**：区分 `Sequential`、`MatchArmBranch`、`LoopBody`、`LoopBackEdge`、`LoopBreakOrExit`、`TryOkContinue`、`TryErrEarlyReturn`、`ReturnJump` 与 `AwaitSuspendResume`。
- **闭包独立子图**：闭包体通过 `nested_closure_cfgs` 独立建模，在外部函数中仅记录 `ClosureCreationSite`，不把延迟执行的闭包体混入外层函数顺序控制流。

### 5.2 源码 CFG 与 MIR / Drop / Unwind 的诚实边界
- `claims_mir_drop_or_unwind_semantics` 恒为 `false`（由 `validate_rust_cfg_contract()` 强制把关）：明确声明本图为源码级结构控制流，不声称已导出 `rustc` 内部 MIR、隐式 `Drop::drop` 析构胶水或 panic 栈展开（unwind）边。
