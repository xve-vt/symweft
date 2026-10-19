# C/C++ 语义构造覆盖矩阵与边界规范

本规范定义 Symweft A2 语义分析引擎对 C11/C17 与 C++17/C++20 核心语法与语义构造的覆盖能力、身份绑定规则、源码锚点、编译变体隔离与错误恢复边界（对应任务清单 `CPP-01` ～ `CPP-07`）。

---

## 1. CPP-01：构造级能力与黄金样本清单

### 1.1 四态能力分级定义

为杜绝将静态分析启发式猜测伪装为编译器确定事实，所有 C/C++ 构造严格按以下四态分级输出：
- **`supported`（确定支持）**：由 libclang 前端解析得到唯一确定的 USR 身份与精确源码范围（如普通函数、重载函数、类继承、显式模板实例、宏展开）。
- **`candidate`（受限候选）**：编译期静态类型已知，但运行期目标取决于多态动态分派；引擎基于已索引 TU 继承图输出候选列表，并显式声明闭世界范围假设（如虚函数调用 `virtual_candidate`）。
- **`unknown`（保留未知）**：静态无法唯一确定目标的构造（如任意函数指针调用 `indirect`、未实例化模板内的依赖型调用 `unresolved`），原样保留调用点位置与未知状态，绝不按同名函数盲猜。
- **`out_of_scope`（明确排除或专案）**：运行期动态链接（`dlsym`）、内联汇编跳转目标，以及归口 `WIN-05` 独立验收的 MSVC `cl.exe` 专属扩展。

### 1.2 C/C++ 构造覆盖矩阵与黄金样本表

| 构造标识 | 类别 | 支持状态 | 身份绑定与域规则 | 源码位置与锚点规则 | 编译变体隔离 | 错误与边界处理 |
|---|---|---|---|---|---|---|
| `c_cpp_functions_and_methods` | `functions` | `supported` | Canonical USR（含参数类型 mangling）；外部符号用 `external` 域，`static` 符号用 `unit_id` 域 | 声明/定义锚定标识符 token；`extent` 覆盖完整函数体 | 多变体按 TU 独立落盘，聚合查询按源码位置去重 | 函数体局部语法错误不影响声明符号提取 |
| `cpp_templates_and_specializations` | `templates` | `supported` | 主模板、显式特化、偏特化与隐式实例各自拥有独立 USR，实例记录 `template_origin` | 主模板与特化锚定源码定义处；实例化关联触发点与主模板 | `Vec<int>` 与 `Vec<double>` 签名独立，绝不按裸名合并 | 单 TU 实例展开上限 1024；未决依赖名保留 `unresolved` |
| `cpp_constructors_destructors_and_operators` | `implicit_and_operators` | `supported` | 构造/析构/转换函数与重载运算符均具独立 USR；细分为 `direct` / `implicit_ctor` / `operator_overload` | 锚定在构造表达式或运算符 token 位置 | 随 TU 配置独立提取 | 编译器合成且无 AST 节点的隐式退栈析构公开列明，不伪造调用点 |
| `cpp_types_inheritance_and_overrides` | `types_and_inheritance` | `supported` | `CXXBaseSpecifier` 绑定派生类至基类 USR（含访问控制与 `virtual`）；`clang_getOverriddenCursors` 绑定重写链 | 继承关系锚定基类子句；`override` 锚定派生类声明 | 跨 TU 汇总基类的全部已知派生类与重写实现 | 未定义的前向声明类不生成悬空继承边 |
| `c_cpp_variables_and_read_write_access` | `variables` | `supported` | 全局变量用外部 USR，局部变量绑定函数与偏移，成员变量绑定所属类 USR | 锚定 `DeclRefExpr` / `MemberRefExpr`，区分 `read` / `write` / `read_write` / `address_of` | 函数引用与宏字面量严格排除在变量写之外 | 复杂宏内左值无法判定时保守标为 `read` |
| `c_cpp_macros_and_generated_code` | `preprocessor` | `supported` | `MacroDefinition` 与 `MacroExpansion` 绑定宏 USR | 同时记录展开点 `location` 与宏定义拼写点 `spelling`；纳入 `build_root` 生成代码 | 不同 `-D/-U` 保持独立 TU；非活动分支保留 A1 语法证据 | 生成头文件更新后经 `input_aliases` 失效并重建包含者 |
| `cpp_virtual_dispatch_candidates` | `dispatch` | `candidate` | `clang_Cursor_isDynamicCall` 识别虚调用，关联基类虚函数及已索引 override 候选 | 锚定多态调用点，目标含基类与各派生类实现位置 | 附带 `closed_world_scope`（已索引 TU 列表） | 动态库或未索引 TU 派生类不在候选内，不冒充唯一目标 |
| `c_cpp_function_pointers_and_indirect_calls` | `dispatch` | `unknown` | 变量/参数/字段函数指针调用标为 `indirect`，模板依赖调用标为 `unresolved` | 保留调用点与被调指针符号引用证据 | 不跨文件按同名函数猜测目标 | 严格保留 `indirect` / `unresolved` 状态 |

## 2. 模板与泛型实例（CPP-02）

- **主模板、特化与实例化角色分离（`TemplateRole`）**：
  - `PrimaryFunctionTemplate`（函数主模板 `template<typename T> T compute(T)`，USR 形如 `c:@FT@>1#Tcompute...`）
  - `PrimaryClassTemplate`（类主模板 `template<typename T> struct Box`，USR 形如 `c:@ST>1#T@Box`）
  - `ExplicitSpecialization`（显式全特化 `template<> double compute<double>(double)`，绑定独立定义符号且保留对主模板的关联）
  - `PartialSpecialization`（类模板偏特化 `template<typename T> struct Box<T*>`，对应 `CXCursor_ClassTemplatePartialSpecialization`）
  - `ImplicitInstantiation`（隐式实例化调用，如 `compute<int>(42)`）
- **双目标绑定与防裸名混淆**：
  - `compute<int>` 与 `compute<double>` 的 USR 分别编码具体类型实参（`<#I>` vs `<#d>`），通过 `TemplateSymbolBinding` 精确区分具体目标与主模板定义，绝不退化为按裸名 `compute` 混淆。
  - 在 AST 遍历 `CXCursor_CallExpr` 时，通过 `clang_getSpecializedCursorTemplate` 同时提取具体实例化目标与 `TemplateInstantiationRef` 主模板引用。
- **实例化展开预算保护（`MAX_TEMPLATE_INSTANTIATIONS_PER_TU = 1024`）**：
  - 单个编译单元最多展开记录 1024 个隐式模板实例化引用；超出预算时置 `budget_exhausted = true` 并停止追加隐式实例化，但始终保留主模板与显式/偏特化定义，防止深层模板元编程耗尽内存。
