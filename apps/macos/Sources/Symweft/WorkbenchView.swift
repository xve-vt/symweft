import AppKit
import SwiftUI

struct WorkbenchView: View {
    @EnvironmentObject private var store: WorkspaceStore
    @EnvironmentObject private var appearance: AppearanceController

    var body: some View {
        NavigationSplitView {
            sidebar
                .navigationSplitViewColumnWidth(min: 210, ideal: 260, max: 380)
        } detail: {
            VStack(spacing: 0) {
                if store.workspace == nil {
                    welcome
                } else {
                    HSplitView {
                        reader.frame(minWidth: 440, maxWidth: .infinity, maxHeight: .infinity)
                        if store.showInspector {
                            inspector.frame(minWidth: 240, idealWidth: 270, maxWidth: 340, maxHeight: .infinity)
                        }
                    }
                }
                Divider()
                statusBar
            }
            .frame(minWidth: 700)
        }
        .navigationTitle(store.workspace?.name ?? "Symweft")
        .navigationSubtitle(store.document?.path ?? "读懂代码，看清系统。")
        .toolbar {
            ToolbarItemGroup {
                Button { store.chooseWorkspace() } label: { Label("打开工作空间", systemImage: "folder.badge.plus") }
                    .help("打开工作空间（⇧⌘O）")
                Button { store.refresh() } label: { Label("刷新文件树", systemImage: "arrow.clockwise") }
                    .disabled(store.workspace == nil || store.isOpening)
                Menu {
                    Picker("外观", selection: $appearance.choice) {
                        ForEach(AppearanceChoice.allCases) { Text($0.title).tag($0) }
                    }
                } label: { Label("外观", systemImage: "circle.lefthalf.filled") }
                .help("外观：\(appearance.choice.title)")
                Button { store.showInspector.toggle() } label: { Label("代码关系与文件信息", systemImage: "sidebar.right") }
                    .help("显示或隐藏代码关系与文件信息")
            }
        }
        .overlay {
            if store.isOpening {
                ProgressView("正在读取工作空间…")
                    .padding(24)
                    .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
            }
        }
        .alert("无法完成操作", isPresented: Binding(
            get: { store.errorMessage != nil && !LaunchOptions.isDiagnostic },
            set: { if !$0 { store.errorMessage = nil } }
        )) {
            Button("好") { store.errorMessage = nil }
        } message: { Text(store.errorMessage ?? "") }
    }

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text("工作空间").font(.headline)
                Spacer()
                if let workspace = store.workspace {
                    Text("\(workspace.fileCount)").font(.caption).foregroundStyle(.secondary)
                }
            }
            .padding(.horizontal, 16).padding(.vertical, 12)
            if let workspace = store.workspace {
                Text(workspace.name.uppercased())
                    .font(.caption.weight(.medium)).foregroundStyle(.secondary)
                    .padding(.horizontal, 16).padding(.bottom, 8)
                FileNavigator(
                    tree: store.fileTree, generation: store.treeGeneration,
                    selection: store.selectedPath, onSelect: store.selectFile
                )
            } else {
                VStack(alignment: .leading, spacing: 8) {
                    Text("尚未打开项目").font(.callout)
                    Text("选择本地目录，开始浏览代码。").font(.caption).foregroundStyle(.secondary)
                    Button("打开目录…") { store.chooseWorkspace() }
                }
                .padding(16)
                Spacer()
            }
            Divider()
            Label("本地阅读 · 无构建任务", systemImage: "internaldrive")
                .font(.caption).foregroundStyle(.secondary).padding(12)
        }
    }

    private var welcome: some View {
        VStack(spacing: 18) {
            Image(systemName: "point.3.connected.trianglepath.dotted")
                .font(.system(size: 50, weight: .light)).foregroundStyle(.tint)
            Text("Symweft").font(.system(size: 32, weight: .semibold, design: .rounded))
            Text("读懂代码，看清系统。").font(.title3).foregroundStyle(.secondary)
            Button("打开工作空间…") { store.chooseWorkspace() }
                .buttonStyle(.borderedProminent).controlSize(.large)
            Text("Python · C/C++ · Rust · Java")
                .font(.callout).foregroundStyle(.secondary)
            Text("原生 macOS 阅读与分析\n按需解析 · 本地运行")
                .font(.caption).foregroundStyle(.tertiary).multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var reader: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: "doc.text").foregroundStyle(.secondary)
                Text(store.document?.path ?? "选择一个文件").font(.callout).lineLimit(1)
                Spacer()
                if store.isReading { ProgressView().controlSize(.small) }
                Text("只读").font(.caption).foregroundStyle(.secondary)
            }
            .padding(.horizontal, 14).frame(height: 36)
            .background(.bar)
            Divider()
            if let document = store.document {
                SourceReader(document: document, navigation: store.sourceNavigation, onCaret: store.moveCaret)
            } else {
                ContentUnavailableView("选择源码文件", systemImage: "doc.text.magnifyingglass",
                    description: Text("从左侧文件树打开 UTF-8 文本。"))
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
    }

    private var inspector: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                Label("代码关系", systemImage: "point.3.connected.trianglepath.dotted").font(.headline)
                if let document = store.document {
                    if ["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx"].contains((document.path as NSString).pathExtension.lowercased()) {
                        SemanticInspector()
                        Divider()
                    }
                    analysisInspector
                    Divider()
                    Text("文件信息").font(.subheadline.weight(.medium))
                    VStack(alignment: .leading, spacing: 12) {
                        property("名称", (document.path as NSString).lastPathComponent)
                        property("类型", document.language ?? "纯文本")
                        property("编码", document.encoding)
                        property("行数", document.lineCount.formatted())
                        property("大小", ByteCountFormatter.string(fromByteCount: Int64(document.sizeBytes), countStyle: .file))
                    }
                }
                if let workspace = store.workspace {
                    Divider()
                    Text("工作空间").font(.subheadline.weight(.medium))
                    Text(workspace.root).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                    ForEach(workspace.languages.keys.sorted(), id: \.self) { language in
                        HStack {
                            Text(language)
                            Spacer()
                            Text("\(workspace.languages[language] ?? 0)").monospacedDigit().foregroundStyle(.secondary)
                        }.font(.caption)
                    }
                    if workspace.truncated || workspace.skippedEntries > 0 {
                        Label(workspace.truncated ? "扫描已达到数量或深度限制" : "已跳过 \(workspace.skippedEntries) 个不可读或链接条目",
                              systemImage: "exclamationmark.circle")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
                Spacer()
            }
            .padding(18)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(Color(nsColor: .controlBackgroundColor))
    }

    @ViewBuilder
    private var analysisInspector: some View {
        if let analysis = store.analysis {
            VStack(alignment: .leading, spacing: 10) {
                scopeCalls
                Divider()
                HStack {
                    Text("文件大纲").font(.subheadline.weight(.medium))
                    Spacer()
                    Text("\(analysis.facts.symbols.count)").font(.caption).foregroundStyle(.secondary)
                }
                if analysis.facts.symbols.isEmpty {
                    Text("未发现已支持的定义类型").font(.caption).foregroundStyle(.secondary)
                }
                LazyVStack(alignment: .leading, spacing: 6) {
                    ForEach(Array(analysis.facts.symbols.prefix(200))) { symbol in
                        Button { store.navigate(to: symbol.nameRange) } label: {
                            HStack(alignment: .firstTextBaseline, spacing: 6) {
                                Image(systemName: symbol.isCallable ? "function" : "square.stack.3d.up")
                                    .foregroundStyle(.secondary)
                                Text(symbol.name).lineLimit(1)
                                Spacer(minLength: 0)
                                Text("\(symbol.range.startLine)").monospacedDigit().foregroundStyle(.secondary)
                            }.font(.caption)
                        }
                        .buttonStyle(.plain)
                        .help("\(symbol.kind) · 第 \(symbol.range.startLine) 行")
                    }
                }
                if analysis.facts.symbols.count > 200 {
                    Text("先显示前 200 项").font(.caption).foregroundStyle(.secondary)
                }
            }
        }
        if let note = store.analysisNote {
            Text(note).font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
        }
    }

    private var scopeCalls: some View {
        VStack(alignment: .leading, spacing: 9) {
            Text(store.currentFunction?.name ?? "文件级调用")
                .font(.subheadline.weight(.medium)).lineLimit(1)
            Text("显式调用点 · 目标待语义解析")
                .font(.caption).foregroundStyle(.secondary)
            let calls = store.currentCalls
            if calls.isEmpty {
                Text("此作用域未发现显式调用点").font(.caption).foregroundStyle(.secondary)
            }
            LazyVStack(alignment: .leading, spacing: 7) {
                ForEach(Array(calls.prefix(200))) { call in
                    Button { store.navigate(to: call.range) } label: {
                        HStack(alignment: .firstTextBaseline) {
                            Text(call.displayName).lineLimit(1)
                            Spacer(minLength: 0)
                            Text("\(call.range.startLine)").monospacedDigit().foregroundStyle(.secondary)
                        }.font(.caption).frame(height: 22)
                    }.buttonStyle(.plain).help(call.callee)
                }
            }
            if calls.count > 200 { Text("先显示前 200 项").font(.caption).foregroundStyle(.secondary) }
            Text("移动光标查看所在函数；点击条目跳转源码。")
                .font(.caption).foregroundStyle(.tertiary)
        }
    }

    private func property(_ name: String, _ value: String) -> some View {
        HStack(alignment: .top) {
            Text(name).foregroundStyle(.secondary).frame(width: 42, alignment: .leading)
            Text(value).textSelection(.enabled)
            Spacer(minLength: 0)
        }.font(.callout)
    }

    private var statusBar: some View {
        HStack(spacing: 14) {
            Label(store.analysis == nil ? "文本阅读" : "语法结构", systemImage: "book").foregroundStyle(.secondary)
            if let update = store.analysisUpdate {
                Text(String(format: "%.2f ms", Double(update.totalMicros) / 1000))
                    .foregroundStyle(.secondary).help("当前文件引擎耗时；不含磁盘与界面，模式：\(update.mode)")
            }
            if let workspace = store.workspace { Text("\(workspace.fileCount) 个文件").foregroundStyle(.secondary) }
            Spacer()
            Text(appearance.choice.title).foregroundStyle(.secondary)
            if let info = store.coreInfo {
                Text("Rust core \(info.version)").foregroundStyle(.secondary)
            } else {
                Text("核心未连接").foregroundStyle(.secondary)
            }
        }
        .font(.caption)
        .padding(.horizontal, 14)
        .frame(height: 28)
        .background(.bar)
    }
}
