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
