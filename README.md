# Symweft · 织源

读懂代码，看清系统。

面向 Python、C/C++、Rust、Java 的原生代码阅读工具与 IDE。共享 Rust 核心，各平台独立原生前端；当前从本机 macOS 开始，Linux/GNOME 后续重点支持。

逐项推进、依赖、验收条件及完成标记见 [任务分解与验收清单](docs/13-任务分解与验收清单.md)。

## 当前可运行范围

- SwiftUI/AppKit 原生工作台、目录选择、文件树、源码阅读、行号与文件信息。
- Rust core 子进程：真实目录扫描、语言标签与受限 UTF-8 文件读取。
- 共享 Rust 分析引擎：Python、C/C++、Rust、Java 的常见定义、作用域和显式调用点；增量语法解析、修订保护和有预算的热缓存。
- macOS 文件大纲、随光标显示当前函数的调用点、点击定位与实际引擎耗时。
- C/C++ 编译配置导入、libclang 语义分析、定义/引用/出入调用、按 TU 持久化及头文件变更更新；macOS 已接入语义面板。
- 原生文件事件驱动的失效与自动重建、持久查询连接、有预算的指纹缓存和严格校验模式；只自动维护已索引范围。
- 未保存单缓冲区的异步 C/C++ 语义预览、最新修订保护、取消、当前 TU 关系查询和单槽内存预算；Swift 客户端接口已验证。
- `a2-query-v1` 统一查询版本显式绑定工作空间、配置/provider、SQLite 基线、文档修订集合和覆盖范围，旧版本组合明确拒绝；`semantic.project.at/relations` 合并未保存 TU 与其他有效磁盘贡献并遮蔽旧边，支持多未保存主/头文件缓冲区同编译参与关系合并、多 TU 有界调度、失败遮蔽、保存基线接续、多文档生命周期隔离、稳定版本有界分页、多变体黄金用例验证与 Swift 客户端深度对齐，旧单 TU 预览保持兼容隔离。
- 外观默认实时跟随系统；设置和工具栏可选择跟随系统/浅色/深色。
- 使用更轻的中性代码区底色；原生控件随系统变化。

当前 UI 仍为只读阅读。多语言语法调用点与 C/C++ 编译器语义分别标注；内存编辑和不落盘的语义预览已通过协议验证。尚未实现用户编辑保存、其他语言的准确语义、跨项目调用/控制流图或调试。Stitch 初稿中的演示数据不作为真实分析结果显示。

已接入 SQLite 持久化 A1 索引和独立 A2 语义库、SMF2 紧凑二进制事实、源码指纹校验和跨重启恢复；PERF 性能优化全系闭环（有界二进制 IPC 传输、Preamble 头部复用、A2 语义热池与出调用预读、活动 TU 二分区间与邻接查询索引、统一资源账本、调度自适应滞回、WAL 批量持久化与孤立垃圾回收，以及原生 UI 空闲周期防抖与重绘消除）；命中时无需创建 AST。设置支持 A1 自动/手动/关闭热缓存、MiB 上限、预读及本地阅读关注收集；HEAT 结构热度与用户关注全系闭环（变体去重调用点/独立调用者、稳定 USR 关注身份与重构迁移、7 天半衰期衰减与隐私清空、热度驱动效用淘汰防大函数挤占、JSON/CSV 标准导出与零污染轨迹回放）。结构候选热度与用户关注分开输出；CPP 语义构造覆盖全系闭环（16 类构造能力矩阵与黄金样本、模板/特化/偏特化与有界实例化、显式/隐式调用与重载运算符分类、类型/继承/虚重写与别名索引、五类变量作用域与四类读写访问防误标、带参宏双锚点与生成头文件/预处理变体隔离、虚分派闭合范围候选集与 Clang/GCC 工具链参数边界）；VAL 规模与可靠性全系闭环（QEMU 2855 TU 固定清单与全量首轮记账、五维失败归因与原编译器对照、大批任务中断秒级续跑、420 TU C++20 模板/继承大型样本、六大场景增量与全量规范化等价、10x/100x 合成存储规模与冷启动读取、SQLite/SMF2 故障自愈与多实例韧性、1000 轮长时回放零泄漏回收及 5 轮稳定性能回归门槛）；A2 编译器工作进程使用单独的超时与内存软预算。

**研发主线为分析引擎：低资源、快速增量更新、即时关系查询优先于扩充 IDE 外壳。**

## 本机运行

需要 macOS 14+、Swift 6+（Command Line Tools 或 Xcode）与 Rust 1.85+。

~~~sh
bash scripts/run-macos.sh
~~~

默认打开项目自身，也可传入其他工作空间：

~~~sh
bash scripts/run-macos.sh /path/to/project
~~~

仅构建：

~~~sh
bash scripts/build-macos.sh
~~~

产物为 build/macos/Symweft.app，可在 Finder 打开。脚本使用本地 ad-hoc 签名用于开发，尚未配置分发证书与公证。

默认离线使用已有 Cargo 缓存。首次在其他机器下载依赖时：

~~~sh
SYMWEFT_OFFLINE=0 bash scripts/build-macos.sh
~~~

发布构建可设置 SYMWEFT_BUILD_CONFIG=release。Swift 包没有第三方依赖。也可用 Xcode 打开 apps/macos/Package.swift，并通过 SYMWEFT_CORE_PATH 环境变量指定编译后的核心。

## 验证

~~~sh
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
python3 scripts/check-semantic.py
python3 scripts/check-semantic-realtime.py
python3 scripts/check-semantic-preview.py
python3 scripts/check-performance.py
bash scripts/check-macos.sh
cargo run --locked --offline --release -p symweft-analysis --example latency
~~~

应用支持自身窗口图层的开发诊断，不捕获用户桌面、不改动系统外观：

~~~sh
build/macos/Symweft.app/Contents/MacOS/Symweft \
  --workspace "$PWD" --appearance light --capture-to /tmp/symweft-light.png
~~~

同样可以使用 dark，或使用 --file 指定初始文件。诊断模式导出 PNG、输出 JSON 状态并退出，不保存测试外观偏好。系统合成材质（例如 macOS 26 的玻璃侧栏）在图层导出中可能简化，PNG 不代替真实交互验收。

使用 --inspect-symbol replacement_edit 可在该文件有相应定义时验证大纲定位；诊断 JSON 同时报告真实符号/调用点数量与当前函数作用域。

## 全量索引与 QEMU 基准

~~~sh
cargo run --locked --offline --release -p symweft-core -- index test_code/qemu
bash scripts/benchmark-qemu.sh
~~~

全量命令流式处理支持的源码文件，单文件预算 2 MiB，批次事务更新；不常驻全库 AST，不运行项目构建脚本。再次执行按内容指纹复用，完整遍历后清理已删除文件。交互程序按需打开，不自动阻塞式扫描全库。`SYMWEFT_CACHE_DIR` 可指定独立缓存根，默认为平台本地缓存目录。

本机 QEMU 6,723 文件：首次完整命令 15.47 秒（索引循环 14.74 秒）、Rust 核心峰值 RSS 35.94 MiB、索引 66.63 MiB；未变化复查 1.29 秒。完整方法、后端对比、缓存代价和精度限制见性能报告；这些数字不代表语义分析或所有超大项目的性能。

## C/C++ 编译器语义

先用项目构建系统生成 `compile_commands.json` 和必要的生成头文件。Symweft 导入配置时不执行其中的编译命令，按需启动受限 libclang 工作进程：

~~~sh
cargo build --locked --offline --release -p symweft-core
./target/release/symweft-core semantic-index /path/to/project /path/to/build/compile_commands.json
~~~

命令后可追加源码相对路径或 `--limit N` 缩小范围。macOS 使用“工作空间 → 导入 C/C++ 编译配置”，再点击“分析当前文件”；关系保留编译单元归属，虚调用和间接调用明确标记不确定性。需要可用的 libclang，自动发现失败时设置 `LIBCLANG_PATH`。

真实 QEMU 的 30 TU 子集：29 成功、1 个与原始编译器一致的源码错误；首轮 16.83 s、重用复查 0.78 s、索引 109.67 MiB，查询 p95 约 23–34 ms。Core 首轮峰值 RSS 149.53 MiB，成功工作进程累计最大峰值 111.03 MiB；不是进程树同一时刻总量。未完成 2,855 TU 全库分析，详见 [C/C++ 语义报告](docs/10-C-Cpp语义闭环与QEMU验证.md)。

以上为首版构建与读取基线。后续已加入原生监听、指纹缓存、自动更新和外部保存后阅读器重载；严格/缓存读取对照及一致性边界见 [实时更新与查询优化](docs/11-实时语义更新与查询优化.md)。macOS 提供“自动更新已索引文件”“严格内容校验”和“刷新已索引范围”；后者强制重建已知范围，不扩展到全库。

未保存内容另有异步预览 API：前台请求可取消/替换旧工作，与索引合计最多一个编译工作进程；仅保留最新 TU 结果，默认 32 MiB、可手动配置 1..128 MiB。QEMU 大型 TU 的交互和保留容量实测、修订一致性及尚未实现的项目级 overlay 合并见 [异步语义预览](docs/12-异步语义预览与修订一致性.md)。当前原生阅读器仍未开放用户编辑。

性能定位可运行 `python3 scripts/benchmark-analysis-pipeline.py`，记录真实 QEMU 子集的阶段、探针开关对照和指定进程树资源。默认运行不启用细粒度计时；测量方法及原始数据见 [流水线与资源基线](docs/14-分析流水线测量与进程资源基线.md)。

## 目录

~~~text
apps/macos/             SwiftUI/AppKit 原生前端
crates/symweft-core/    可移植 Rust 工作空间与协议
crates/symweft-analysis/ 共享交互分析、预算、语法事实与增量测试
crates/symweft-index/   SQLite/事实块、热缓存、热度及实验分段后端
crates/symweft-semantic/ C/C++ 编译语义、TU 依赖、工作进程与紧凑关系存储
scripts/               本地构建、打包、运行
docs/                  产品、UI、开发记录与协议
ui_desgin/             Stitch 设计初稿（保留原目录名）
~~~

## 设计与约束

- [产品与技术设计](docs/01-产品与技术设计.md)
- [UI 设计交付要点](docs/02-UI设计交付要点.md)
- [最新开发记录与主题修订](docs/03-开发记录.md)
- [本地核心协议](docs/04-本地核心协议.md)
- [流水线测量与进程资源基线](docs/14-分析流水线测量与进程资源基线.md)
- [分析引擎与实时关系设计](docs/05-分析引擎与实时关系设计.md)
- [本机语法微基准原始数据](docs/06-语法性能基线-2026-10-18.json)
- [分析结果持久化与快速查询](docs/07-分析结果持久化与快速查询.md)
- [大规模索引与自适应缓存评估](docs/08-大规模索引与自适应缓存评估.md)
- [QEMU 性能与存储选型报告](docs/09-QEMU性能与存储选型报告.md)
- [C/C++ 语义闭环与 QEMU 验证](docs/10-C-Cpp语义闭环与QEMU验证.md)
- [实时语义更新与查询优化](docs/11-实时语义更新与查询优化.md)
- [异步语义预览与修订一致性](docs/12-异步语义预览与修订一致性.md)
- [任务分解与验收清单](docs/13-任务分解与验收清单.md)

ui_desgin 中 HTML/PNG 是 Stitch 初稿，作为信息结构和交互参考；实现遵循最新的原生 macOS 与系统主题要求。软件发布许可尚未指定。
