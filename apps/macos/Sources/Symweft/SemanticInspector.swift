import SwiftUI

struct SemanticInspector: View {
    @EnvironmentObject private var store: WorkspaceStore
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("C/C++ 语义").font(.subheadline.weight(.medium))
            if store.semanticConfigured {
                HStack {
                    Button("分析当前文件") { store.analyzeCurrentSemantics() }.disabled(store.semanticRunning)
                    if store.semanticRunning { Button("取消") { store.cancelSemanticAnalysis() } }
                }.controlSize(.small)
                Button("刷新已索引范围") { store.analyzeCurrentSemantics(refreshIndexed: true) }
                    .controlSize(.small).disabled(store.semanticRunning)
                    .help("强制重建已分析的编译单元；不会扩展到整个编译数据库。")
                Toggle("自动更新已索引文件", isOn: $store.semanticAutoRefresh)
                    .font(.caption).onChange(of: store.semanticAutoRefresh) { _, _ in store.updateSemanticWatchSettings() }
                Toggle("严格内容校验", isOn: $store.semanticStrictValidation)
                    .font(.caption).onChange(of: store.semanticStrictValidation) { _, _ in store.updateSemanticWatchSettings() }
                    .help("每次读取全部输入内容；关闭时使用原生事件、文件属性和有时限的指纹缓存。")
                if store.semanticWatch?.paused == true {
                    Button("恢复自动更新") { store.updateSemanticWatchSettings() }.controlSize(.small)
                }
                if store.semanticFunctions.count > 1 {
                    Text("选择编译单元").font(.caption).foregroundStyle(.secondary)
                    ForEach(store.semanticFunctions, id: \.unitId) { function in
                        Button("\(((function.mainFile ?? "") as NSString).lastPathComponent) · \(String((function.unitId ?? "").prefix(8)))") {
                            store.semanticUnitID = function.unitId; store.queueSemanticContext()
                        }.font(.caption)
                    }
                }
                if let entity = store.semanticEntity {
                    Button("定位当前符号定义") { store.revealSemanticDefinition(entity) }.controlSize(.small)
                }
                if let function = store.semanticFunction {
                    Text(function.name).font(.caption.monospaced()).textSelection(.enabled)
                    calls("入调用", rows: store.semanticIncoming, incoming: true)
                    calls("出调用", rows: store.semanticOutgoing, incoming: false)
                    Text("结果来自已索引编译单元；虚调用候选与间接调用不代表确定运行目标。")
                        .font(.caption2).foregroundStyle(.secondary)
                } else {
                    Text("将光标放入函数；无快照时先分析当前文件。")
                        .font(.caption).foregroundStyle(.secondary)
                }
                ForEach(Array(store.semanticDefinitions.enumerated()), id: \.offset) { _, symbol in
                    Button("\(symbol.name) · \((symbol.location.path as NSString).lastPathComponent):\(symbol.location.line)") {
                        store.openSemanticLocation(symbol.location)
                    }.font(.caption).buttonStyle(.plain)
                }
            } else {
                Button("导入编译配置…") { store.chooseCompileCommands() }.controlSize(.small)
                Text("使用真实构建配置解析定义、引用和调用目标。")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let notice = store.semanticNotice {
                Text(notice).font(.caption2).foregroundStyle(.secondary).lineLimit(4).help(notice)
            }
        }
    }
    private func calls(_ title: String, rows: [SemanticCall], incoming: Bool) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("\(title) · \(rows.count)").font(.caption.weight(.medium))
            ForEach(Array(rows.prefix(30).enumerated()), id: \.offset) { _, call in
                Button { store.openSemanticLocation(call.location) } label: {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(incoming ? call.callerName ?? "全局/未知调用者" : call.name)
                            .font(.caption.monospaced()).lineLimit(1)
                        Text("\(call.resolutionLabel) · \((call.location.path as NSString).lastPathComponent):\(call.location.line)")
                            .font(.caption2).foregroundStyle(.secondary).lineLimit(1)
                    }
                }
                .buttonStyle(.plain)
                .contextMenu {
                    Button("打开调用点") { store.openSemanticLocation(call.location) }
                    if let target = call.target { Button("定位目标定义") { store.revealSemanticDefinition(target) } }
                }
                .help("\(call.mainFile ?? "")\n\(call.resolutionLabel)")
            }
            if rows.isEmpty { Text("暂无已索引关系").font(.caption2).foregroundStyle(.secondary) }
            if rows.count > 30 { Text("先显示前 30 项，查询上限为 100 项。") .font(.caption2).foregroundStyle(.secondary) }
        }
    }
}
