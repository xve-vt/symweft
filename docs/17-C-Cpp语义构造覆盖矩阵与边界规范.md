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

## 3. 隐式调用和运算符（CPP-03）

- **显式与隐式调用分类（`CallSyntaxKind`）**：
  - `ExplicitCall`：常规函数/方法调用、显式同名构造调用 `Widget(1)` 或显式 `obj.operator+(other)`。
  - `ImplicitConstructor`：声明触发的隐式构造或隐式拷贝/转换构造（libclang 生成无表面函数名的 `CXCursor_CallExpr`，由 `provider.rs` 回填目标构造函数符号名）。
  - `ImplicitConversion`：隐式触发的用户定义类型转换函数（`CXCursor_ConversionFunction`，如 `operator bool()`）。
  - `OverloadedOperator`：通过中缀/前缀/后缀或下标/括号语法触发的重载运算符调用（`a + b`、`obj[i]`、`fn()`）。
  - `ExplicitDestructorOrDelete`：显式析构调用 `p->~Widget()` 或 `delete p`（`CXCursor_CXXDeleteExpr`）。
  - `UntrackedCompilerSynthesized`：编译器合成或栈展开自动调用。
- **未追踪边明确说明（`untracked_implicit_edges_catalog`）**：
  - `scope_exit_automatic_destructor`：块作用域退出或异常栈展开时的局部变量自动析构，libclang AST 不生成 `CallExpr` 节点；系统如实记录析构函数声明/定义与显式 `~T()`/`delete` 调用，绝不按右花括号位置伪造栈退出调用边。
  - `trivial_compiler_defaulted_special_members`：无用户源码定义的编译器合成 trivial 默认/拷贝/移动特殊成员函数不生成跨文件调用边。
  - `uninstantiated_dependent_operator`：未实例化模板体内的依赖型运算符表达式不猜测目标重载。

## 4. 类型、继承与实现关系（CPP-04）

- **类型与别名统一分类（`CppTypeKind`）**：
  - 完整覆盖 `Class`、`Struct`、`Union`、`Enum`、`EnumConstant`、`TypedefAlias`（`CXCursor_TypedefDecl`）与 `UsingTypeAlias`（`CXCursor_TypeAliasDecl` / `CXCursor_TypeAliasTemplateDecl`）。
  - 在 `provider.rs` 中，针对 `TypedefDecl` 与 `TypeAliasDecl` 通过 `clang_getTypedefDeclUnderlyingType` + `clang_getCanonicalType` 解析底层被别名类型声明，并生成 `TypeAliasUnderlyingRef` 引用关系。
- **基类与派生类双向查询（`InheritanceRelation`）**：
  - 遍历 `CXCursor_CXXBaseSpecifier` 提取基类引用，记录派生类、基类、访问控制（`public`/`protected`/`private`）及是否虚继承（`is_virtual`），支持 `base_classes_of` 与 `derived_classes_of` 双向检索。
- **虚函数重写关系（`VirtualOverrideRelation`）**：
  - 在 `CXCursor_CXXMethod` 上调用 `clang_getOverriddenCursors` 提取所有直接被重写的基类虚函数，写入 `CXXMethodOverride` 关系，支持查询子类重写了哪个基类方法（`overridden_by`）以及基类虚函数有哪些子类实现（`overrides_of`）。
- **成员声明与类外定义关联（`MemberDeclarationRelation`）**：
  - 类内成员声明与类外定义（`void FileStream::read()`）通过规范化 USR 归并至同一实体 ID，分别记录 `definition = false` 与 `definition = true`。

## 5. 变量与读写关系（CPP-05）

- **五类变量存储与作用域（`VariableStorageScope`）**：
  - `Local`（函数/块内局部变量）
  - `Global`（编译单元或命名空间级全局变量）
  - `StaticInternal`（`static` 内部链接变量或类静态存储变量）
  - `Parameter`（函数形参 `CXCursor_ParmDecl`）
  - `MemberField`（结构体/类/联合体成员变量 `CXCursor_FieldDecl`）
- **四类访问模式稳定分类（`VariableAccessMode`）**：
  - `Read`（右值读取、实参传递、条件判断）
  - `Write`（简单赋值 `=` 左操作数）
  - `ReadWrite`（复合赋值 `+=`/`-=`/`|=` 左操作数及前缀/后缀 `++`/`--`）
  - `AddressOf`（一元取地址 `&var`，标记潜在间接访问）
- **严格防误标保护**：
  - `classify_variable_scope` 与 `classify_variable_access` 首先校验目标游标必须属于 `VarDecl`、`ParmDecl` 或 `FieldDecl`；对于函数调用（`FunctionDecl`/`CXXMethod`，哪怕返回左值引用出现在 `=` 左侧如 `slot() = 1`）、宏定义/宏字面量（`MacroDefinition`）及枚举常量（`EnumConstantDecl`），一律返回 `None`，绝不将函数调用或宏字面量误标为变量写操作。

## 6. 宏、生成代码和预处理变体（CPP-06）

- **带参宏双锚点（`MacroDualAnchor`）**：
  - 宏展开生成的调用与符号引用同时记录 `expansion_location`（调用点宏展开位置）与 `spelling_location`（宏定义体或宏实参的源码拼写位置），支持在展开点与拼写点之间双向定位。
- **构建目录生成头文件追踪与重建失效（`GeneratedHeaderTracker`）**：
  - `Walker::project` 与 `GeneratedHeaderTracker` 同时将 `workspace_root` 与 `build_root`（如 `build/generated/config.h`、`autoconf.h`）纳入项目追踪范围。
  - 生成头文件写入 `inputs` 与 `input_aliases` 指纹表，当构建系统重新生成头文件导致内容哈希变化时，通过 `should_invalidate_on_rebuild` 自动使所有依赖该生成头文件的 TU 失效。
- **条件编译与 `-D` 变体隔离（`PreprocessorVariantIdentity`）**：
  - 同一源码文件在不同 `-D`/`-U` 编译宏组合下拥有独立的 `unit_id`（`variant_unit_id`），在 SQLite 中按 `(unit_id, configuration)` 独立存储并在查询时标注变体来源，绝不相互覆盖。
- **未激活预处理分支策略（`InactiveBranchPolicy`）**：
  - 对于当前 TU 未激活的 `#ifdef` / `#if 0` 分支，保留 A1 词法/结构大纲供阅读，绝不凭空猜测或伪造 A2 编译器语义边（`a2_semantic_edges_emitted = false`）。

## 7. 分派候选与工具链边界（CPP-07）

- **虚调用候选集与闭合世界假设（`VirtualDispatchCandidateSet`）**：
  - 虚调用在编译期记录为 `resolution: "virtual_candidate"`，通过 `VirtualDispatchCandidateSet::from_hierarchy` 聚合所有已知子类重写方法（`candidate_override_ids`）。
  - 强制附带 `is_exact = false`、`closed_world_scope`（`all_known_compdb_units_indexed` 或 `partial_indexed_translation_units`）及显式的 `assumption_note`（声明当前候选集基于 `M/N` 已索引 TU 的闭合范围假设，动态加载或未索引单元可能包含额外重写），绝不将虚分派候选冒充为唯一确定性目标。
- **函数指针与未解析调用诚实标注（`DispatchResolutionKind`）**：
  - 函数指针、成员函数指针与回调变量调用始终保持 `resolution: "indirect"`（`IndirectFunctionPointer`），无法解析的依赖表达式保持 `resolution: "unresolved"`（`Unresolved`），绝不通过名称猜测目标函数。
- **Clang/GCC 与 MSVC 工具链参数边界（`normalize_toolchain_arguments`）**：
  - **Clang / GCC 支持范围**：完整保留头文件搜索路径（`-I`、`-isystem`、`-iquote`、`--sysroot`、`-isysroot`）、宏与强制包含（`-D`、`-U`、`-include`）、语言标准与目标特性（`-std=`、`-f...`、`-m...`、`-pthread`、`-x`）；自动剥离仅用于生成构建依赖或 GCC 驱动专有的干扰参数（`-MMD`、`-MP`、`-MF <file>`、`-MT <target>`、`-fno-canonical-system-headers`、`-Wa,...`、`-Wl,...`、`-c`、`-o <file>`）。
  - **MSVC 边界**：识别 `cl.exe` / `clang-cl` 及 `/I`、`/D`、`/std:` 风格参数时明确标记为 `ToolchainKind::MsvcDeferredToWin05`，将 MSVC 命令行与编译数据库适配留给 `WIN-05` 阶段独立验收。
