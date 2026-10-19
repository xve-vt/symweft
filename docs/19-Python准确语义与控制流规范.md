# 19-Python 准确语义与控制流规范（PY-01 ～ PY-06）

本文档定义 Symweft 针对 Python 项目的静态准确语义分析、作用域与跨模块绑定、提供器增量调度及源码控制流图（CFG）规范。

## 1. 环境与模块发现（PY-01）

### 1.1 可追踪环境身份（`PythonEnvironmentIdentity`）
- **解释器与虚拟环境识别**：显式记录 `interpreter_path`、`interpreter_version`、`venv_kind`（`none` / `dot_venv` / `venv` / `conda` / `poetry` / `pipenv` / `custom`）与 `venv_root`。
- **源码布局与多根搜索路径**：自动识别 `src/` 布局（`SrcLayout`）与扁平布局（`FlatLayout`），按确定性优先级构造 `search_paths`（`typings` / `stubs` $\rightarrow$ `src` $\rightarrow$ 工作区根 `.` $\rightarrow$ `site-packages`）。
- **静态项目配置摘要**：对 `pyproject.toml`、`setup.cfg`、`pyrightconfig.json`、`mypy.ini` 计算内容 SHA-256 指纹并纳入 `environment_fingerprint()`，任何配置或解释器变化均生成新的可追踪身份。
- **模块、PEP 420 Namespace Package 与 `.pyi` Stub 规则**：
  - 含有 `__init__.py` 的目录解析为 `RegularPackage`；
  - 不含 `__init__.py` 但包含子模块的目录（可跨多个搜索根聚合）按 PEP 420 解析为 `NamespacePackagePep420`，完整保留 `namespace_roots`；
  - 同名 `.pyi` 类型存根（无论位于 `typings/` 还是同目录）静态类型优先级高于 `.py`，标记为 `TypeStubModule`，同时通过 `runtime_implementation_path` 关联底层 `.py` 实现文件。
- **静态安全隔离红线**：`executes_user_code` 恒为 `false`，并在 `validate_static_isolation()` 中强制校验，严禁在分析期间通过 `import`、`setup.py` 或子进程执行任何用户 Python 模块代码。

## 2. 作用域与 Import 绑定（PY-02）

### 2.1 LEGB 词法作用域与类作用域跳过规则（`PythonLexicalScopeTree`）
- 作用域节点显式区分为 `LocalFunction`（函数/推导式/Lambda 局部）、`EnclosingClosure`（外层闭包函数）、`ClassBody`（类定义体）、`ModuleGlobal`（模块全局）与 `Builtin`。
- **类体穿透规则**：方法或嵌套函数在向上查找非限定名时，严格遵循 Python 语言规范跳过外层 `ClassBody` 作用域（除非通过 `self.attr` 或 `ClassName.attr` 显式属性访问），避免将同名类属性误绑为方法闭包变量。
- **显式声明绑定**：`nonlocal x` 强制绑定至最近的外层 `LocalFunction` 作用域；`global y` 直接绑定至根 `ModuleGlobal` 作用域。

### 2.2 绝对/相对 Import、别名与冲突/缺依赖诊断（`PythonImportBinding`）
- 支持常规模块导入、符号导入（`from ... import sym as alias`）、子模块导入以及多级相对导入（`.` / `..` / `...`，自动区分普通模块与 `__init__.py` 锚点）。
- **明确边界标注**：
  - `Resolved`：成功定位目标模块与导出符号，并记录是否命中 `.pyi` 存根（`via_stub`）；
  - `AmbiguousConflict`：多搜索根存在同名遮蔽冲突时列出全部候选路径（`candidate_paths`）；
  - `MissingDependency`：当相对导入越过顶层包边界（`beyond top-level package`）、第三方依赖未安装或导出符号缺失时，明确记录诊断原因，绝不静默忽略。

## 3. 跨模块定义、引用与调用及动态边界（PY-03）

### 3.1 跨模块绑定与共用 A2 查询体系（`PythonModuleSemanticModel`）
- 每个 Python 符号拥有稳定的跨模块身份 `py-usr:<module>:<qualname>`（例如 `py-usr:auth:Authenticator.verify`）。
- 可解析的跨模块函数调用（`DirectFunctionCall`）与已知接收者类型的绑定方法调用（`BoundMethodCall`，包括 `self`、`cls`、构造函数推断或显式类型注解）生成 `dispatch = "direct"` 的 `CallSite` 与 `Reference`，并转换为标准 `crate::model::Snapshot` 写入共用 SQLite `Store`，直接复用 `semantic.at` / `semantic.relations` / `semantic.project.relations` 查询体系。

### 3.2 动态属性、反射与运行时替换诚实边界（绝不同名猜测）
- **未知接收者动态属性调用（`DynamicAttributeUnresolved`）**：当接收者缺少静态类型或为动态参数（如 `req.verify()`）时，严禁在全库按同名方法 `verify` 猜测绑定目标，调用边标记为 `target = None, dispatch = "unresolved"`。
- **反射与动态执行（`ReflectionOrEvalUnresolved`）**：`getattr(obj, name)()`、`setattr`、`eval`、`exec`、`globals()[k]()` 统一归类为 `target = None, dispatch = "indirect"`。
- **运行时猴子补丁（`RuntimeMonkeyPatchUnresolved`）**：运行期动态替换类或模块属性（如 `Service.run = hook`）明确标记为 `unresolved`，不伪造静态调用目标。
