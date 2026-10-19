//! C/C++ construct capability matrix, semantic extraction extensions, and toolchain boundary rules (CPP-01 ~ CPP-07).

use serde::{Deserialize, Serialize};

/// Support level for each C/C++ language construct in the A2 semantic engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstructSupportStatus {
    /// Deterministic compiler-backed target binding and source location.
    Supported,
    /// Bounded candidate set with explicit closed-world scope assumptions (e.g., virtual dispatch).
    Candidate,
    /// Preserved as indirect or unresolved without guessing targets (e.g., function pointers, dependent types).
    Unknown,
    /// Explicitly outside static analysis guarantees or deferred (e.g., MSVC-specific flags in WIN-05).
    OutOfScope,
}

/// Detailed specification of how a C/C++ language construct is bound, anchored, isolated across variants, and bounded on error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConstructCapabilityEntry {
    pub construct_id: String,
    pub category: String,
    pub title: String,
    pub status: ConstructSupportStatus,
    pub binding_identity_rule: String,
    pub location_anchor_rule: String,
    pub variant_isolation_rule: String,
    pub error_boundary_rule: String,
    pub compiler_flags: Vec<String>,
    pub golden_sample: String,
}

/// Returns the canonical C/C++ construct capability matrix and golden sample catalog (CPP-01).
pub fn construct_capability_matrix() -> Vec<ConstructCapabilityEntry> {
    vec![
        ConstructCapabilityEntry {
            construct_id: "c_cpp_functions_and_methods".into(),
            category: "functions".into(),
            title: "C 函数、C++ 成员函数与重载函数".into(),
            status: ConstructSupportStatus::Supported,
            binding_identity_rule: "基于 canonical CXCursor 的 Clang USR（含参数类型签名 mangling）；外部链接使用 external 域，static 内部链接绑定 unit_id 域".into(),
            location_anchor_rule: "声明与定义锚定在标识符 token 物理偏移区间；extent 覆盖完整函数体".into(),
            variant_isolation_rule: "不同编译宏变体按各自 TU 存储，跨 TU 查询按源码位置去重或保留变体归属".into(),
            error_boundary_rule: "函数体内局部语法错误不影响函数声明符号提取；缺失返回类型降级为恢复签名".into(),
            compiler_flags: vec!["-std=c++20".into(), "-Wall".into()],
            golden_sample: "namespace ns { int calc(int a); double calc(double a); }".into(),
        },
        ConstructCapabilityEntry {
            construct_id: "cpp_templates_and_specializations".into(),
            category: "templates".into(),
            title: "函数模板、类模板、显式特化与偏特化".into(),
            status: ConstructSupportStatus::Supported,
            binding_identity_rule: "主模板使用 FunctionTemplate/ClassTemplate USR；显式特化与实例化保留特化实参 USR 并关联 template_origin，单 TU 展开上限 1024".into(),
            location_anchor_rule: "主模板与显式特化锚定在各自源码定义点；隐式实例化锚定在触发点并指向主模板定义".into(),
            variant_isolation_rule: "不同类型实参实例（如 Vec<int> 与 Vec<double>）保持独立 USR，绝不按裸名合并".into(),
            error_boundary_rule: "未实例化模板中的依赖名（dependent name）保持 Unknown 状态，不伪造绑定；超出实例上限时记入截断诊断".into(),
            compiler_flags: vec!["-std=c++20".into()],
            golden_sample: "template<typename T> T add(T a, T b) { return a + b; }\ntemplate<> int add<int>(int a, int b) { return a + b + 1; }".into(),
        },
        ConstructCapabilityEntry {
            construct_id: "cpp_constructors_destructors_and_operators".into(),
            category: "implicit_and_operators".into(),
            title: "构造函数、析构函数、类型转换函数与重载运算符".into(),
            status: ConstructSupportStatus::Supported,
            binding_identity_rule: "构造/析构/转换函数与 operator 重载均具备独立 Clang USR；调用按 direct / implicit_ctor / operator_overload 分类".into(),
            location_anchor_rule: "显式调用与重载运算符锚定在运算符或构造表达式 token；离开作用域的编译器合成平凡析构不伪造源码调用点".into(),
            variant_isolation_rule: "随所在 TU 编译配置独立提取".into(),
            error_boundary_rule: "编译器自动合成且无 AST 调用节点的隐式退栈析构公开列为不伪造显式调用点".into(),
            compiler_flags: vec!["-std=c++20".into()],
            golden_sample: "struct Box { Box(int v); ~Box(); Box operator+(const Box& rhs) const; explicit operator bool() const; };".into(),
        },
        ConstructCapabilityEntry {
            construct_id: "cpp_types_inheritance_and_overrides".into(),
            category: "types_and_inheritance".into(),
            title: "类/结构体/枚举、继承关系、虚函数重写与类型别名".into(),
            status: ConstructSupportStatus::Supported,
            binding_identity_rule: "CXXBaseSpecifier 记录派生类到基类 USR 及 public/protected/private 与 virtual 属性；clang_getOverriddenCursors 记录重写链".into(),
            location_anchor_rule: "继承说明符锚定在基类子句位置；override 方法锚定在派生类方法声明处".into(),
            variant_isolation_rule: "跨 TU 聚合基类所有已知派生类与重写实现".into(),
            error_boundary_rule: "前向声明未定义基类仅记录声明符号，不生成悬空继承边".into(),
            compiler_flags: vec!["-std=c++20".into()],
            golden_sample: "struct Base { virtual void run(); }; struct Derived : public Base { void run() override; }; using Runner = Derived;".into(),
        },
        ConstructCapabilityEntry {
            construct_id: "c_cpp_variables_and_read_write_access".into(),
            category: "variables".into(),
            title: "局部变量、全局变量、成员变量与读写分类".into(),
            status: ConstructSupportStatus::Supported,
            binding_identity_rule: "全局变量使用外部 USR，局部/参数变量绑定所属函数与文件偏移域，成员变量绑定所属类 USR".into(),
            location_anchor_rule: "引用锚定在 DeclRefExpr / MemberRefExpr 位置，并按上下文分类为 read / write / read_write / address_of".into(),
            variant_isolation_rule: "函数引用与宏文本字面量严格排除在变量写入之外".into(),
            error_boundary_rule: "复杂宏拼接无法判定左值时保守归类为 read 引用，绝不误判函数调用为变量写入".into(),
            compiler_flags: vec!["-std=c++20".into()],
            golden_sample: "int g_count = 0; void bump(int& p) { g_count += 1; p = g_count; int* ptr = &g_count; }".into(),
        },
        ConstructCapabilityEntry {
            construct_id: "c_cpp_macros_and_generated_code".into(),
            category: "preprocessor".into(),
            title: "宏定义、宏展开、生成代码与条件编译变体".into(),
            status: ConstructSupportStatus::Supported,
            binding_identity_rule: "MacroDefinition 与 MacroExpansion 关联宏定义 USR；同时保留展开位置（location）与宏拼写源位置（spelling）".into(),
            location_anchor_rule: "展开点指向调用宏的源码位置，spelling 指向宏体内部 token 原位置；build_root 下的生成头文件纳入工程范围".into(),
            variant_isolation_rule: "不同 -D/-U 配置生成不同 command_key；非活动 #ifdef 分支保留 A1 语法大纲证据，不伪造 A2 语义事实".into(),
            error_boundary_rule: "生成头文件重建后通过 input_aliases 触发包含者失效与增量重分析".into(),
            compiler_flags: vec!["-std=c11".into(), "-DFEATURE_ENABLED=1".into()],
            golden_sample: "#define CALL_TARGET(fn, x) fn((x) + 1)\nint target(int v); int run(int a) { return CALL_TARGET(target, a); }".into(),
        },
        ConstructCapabilityEntry {
            construct_id: "cpp_virtual_dispatch_candidates".into(),
            category: "dispatch".into(),
            title: "C++ 虚函数动态分派候选集".into(),
            status: ConstructSupportStatus::Candidate,
            binding_identity_rule: "clang_Cursor_isDynamicCall 识别虚调用，结合继承重写索引展开为基类方法及已索引派生类 override 候选列表".into(),
            location_anchor_rule: "调用点锚定在多态调用表达式位置，目标指向基类虚函数与各候选重写实现位置".into(),
            variant_isolation_rule: "候选集显式附带 closed_world_scope（已索引 TU 集合）与假设声明".into(),
            error_boundary_rule: "未索引 TU 或动态加载插件中的派生类不在候选集内，绝不标榜为运行期确定唯一目标".into(),
            compiler_flags: vec!["-std=c++20".into()],
            golden_sample: "void execute(Base* b) { b->run(); } // resolution: virtual_candidate".into(),
        },
        ConstructCapabilityEntry {
            construct_id: "c_cpp_function_pointers_and_indirect_calls".into(),
            category: "dispatch".into(),
            title: "函数指针、成员指针与未实例化依赖调用".into(),
            status: ConstructSupportStatus::Unknown,
            binding_identity_rule: "通过变量/字段/参数发起的间接调用标记为 resolution: indirect；未决模板调用标记为 resolution: unresolved".into(),
            location_anchor_rule: "保留调用点位置、所属调用者函数与被调指针变量引用证据".into(),
            variant_isolation_rule: "不跨文件按同名函数猜测潜在目标".into(),
            error_boundary_rule: "严格保留 indirect / unresolved 未知状态，绝不混淆为确定性入调用边".into(),
            compiler_flags: vec!["-std=c11".into()],
            golden_sample: "typedef int (*op_fn)(int); int apply(op_fn f, int v) { return f(v); } // resolution: indirect".into(),
        },
    ]
}
