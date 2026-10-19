import AppKit
import UniformTypeIdentifiers

extension WorkspaceStore {
    var semanticFunctions: [SemanticSymbol] {
        var units = Set<String>()
        return (semanticContextValue?.enclosingFunctions ?? []).filter { units.insert($0.unitId ?? $0.id).inserted }
    }
    var semanticFunction: SemanticSymbol? {
        let functions = semanticFunctions
        if let unit = semanticUnitID { return functions.first { $0.unitId == unit } }
        return functions.count == 1 ? functions.first : nil
    }
    var semanticEntity: String? {
        guard let context = semanticContextValue else { return nil }
        if let unit = semanticUnitID {
            return context.references.first { $0.unitId == unit }?.target ?? context.declarations.first { $0.unitId == unit }?.id
        }
        let ids = Set(context.references.map(\.target) + context.declarations.map(\.id))
        return ids.count == 1 ? ids.first : nil
    }
    func resetSemantics() {
        semanticGeneration += 1
        semanticWatch = nil; semanticQueryRevision = 0; semanticWatchSettingsPending = false
        semanticTask?.cancel(); semanticPoll?.cancel()
        semanticConfigured = false; semanticRunning = false; semanticNotice = nil
        semanticContextValue = nil; semanticIncoming = []; semanticOutgoing = []; semanticDefinitions = []; semanticUnitID = nil
    }
    func chooseCompileCommands() {
        guard workspace != nil else { return }
        let panel = NSOpenPanel()
        panel.title = "导入 C/C++ 编译配置"
        panel.message = "选择构建系统生成的 compile_commands.json。导入后按需分析，不执行其中的编译命令。"
        panel.allowedContentTypes = [.json]; panel.allowsMultipleSelection = false
        panel.begin { [weak self] response in
            guard response == .OK, let url = panel.url else { return }
            Task { @MainActor in await self?.configureSemantics(url) }
        }
    }
    func configureSemantics(_ url: URL) async {
        guard workspace != nil else { return }
        resetSemantics()
        let generation = semanticGeneration
        do {
            let status = try await core.configureSemantics(path: url.path)
            guard generation == semanticGeneration else { return }
            semanticConfigured = status.configured
            semanticNotice = "已导入 \(status.configuredUnits ?? 0) 个编译单元；按需分析。"
            if let count = status.incompatibleProviderUnits, count > 0 {
                semanticNotice = "已导入 \(status.configuredUnits ?? 0) 个编译单元；\(count) 个已有快照与当前编译器或环境不兼容，请重新分析。"
            }
            semanticWatch = try await core.watchSemantics(autoRefresh: semanticAutoRefresh, strict: semanticStrictValidation)
            await loadSemanticContext()
            startSemanticPolling()
        } catch { if generation == semanticGeneration { semanticNotice = error.localizedDescription } }
    }
    func queueSemanticContext() {
        semanticTask?.cancel()
        semanticIncoming = []; semanticOutgoing = []; semanticDefinitions = []
        semanticContextValue = nil
        guard semanticConfigured else { return }
        semanticTask = Task {
            try? await Task.sleep(for: .milliseconds(180))
            guard !Task.isCancelled else { return }
            await loadSemanticContext()
        }
    }
    func loadSemanticContext() async {
        guard semanticConfigured, let path = document?.path else { return }
        let generation = semanticGeneration; let byte = caretByte; let revision = semanticQueryRevision
        do {
            let context = try await core.semanticContext(path: path, byte: byte)
            guard !Task.isCancelled, generation == semanticGeneration, revision == semanticQueryRevision, document?.path == path, caretByte == byte else { return }
            semanticContextValue = context
            guard let function = semanticFunction else { return }
            let outgoing = try await core.semanticCalls(entity: function.id, direction: "outgoing")
            let incoming = try await core.semanticCalls(entity: function.id, direction: "incoming")
            guard !Task.isCancelled, generation == semanticGeneration, revision == semanticQueryRevision, document?.path == path, caretByte == byte else { return }
            semanticOutgoing = outgoing.rows.filter { $0.unitId == function.unitId }
            semanticIncoming = incoming.rows
        } catch { if !Task.isCancelled, generation == semanticGeneration { semanticNotice = error.localizedDescription } }
    }
    func updateSemanticWatchSettings() {
        semanticWatchSettingsPending = true
        if !semanticRunning { Task { await applySemanticWatchSettings() } }
    }
    private func applySemanticWatchSettings() async {
        let generation = semanticGeneration
        do {
            let watch = try await core.watchSemantics(autoRefresh: semanticAutoRefresh, strict: semanticStrictValidation)
            guard generation == semanticGeneration else { return }
            semanticWatch = watch; semanticWatchSettingsPending = false
            semanticQueryRevision += 1
            queueSemanticContext()
        } catch { if generation == semanticGeneration { semanticNotice = "更新设置将在任务完成后应用。" } }
    }
    private func startSemanticPolling() {
        semanticPoll?.cancel()
        let generation = semanticGeneration
        semanticPoll = Task {
            while !Task.isCancelled, generation == semanticGeneration {
                var isIdle = true
                do {
                    let status = try await core.semanticProgress()
                    guard !Task.isCancelled, generation == semanticGeneration else { return }
                    let wasRunning = semanticRunning
                    let newRunning = status.job?.state == "running"
                    if semanticRunning != newRunning {
                        semanticRunning = newRunning
                    }
                    isIdle = !newRunning && (status.watch?.pendingUnits ?? 0) == 0
                    let changed = status.watch?.revision != semanticWatch?.revision
                    if semanticWatch != status.watch {
                        semanticWatch = status.watch
                    }
                    var newNotice: String? = nil
                    if status.watch?.configurationRequired == true {
                        newNotice = "编译配置已变化，请重新导入。"
                    } else if status.watch?.manualRefreshRequired == true {
                        newNotice = "变更影响范围超过自动预算，旧关系已失效，请手动刷新。"
                    } else if status.watch?.pressurePaused == true {
                        newNotice = "内存压力较高，已暂停后台语义更新。"
                    } else if let error = status.watch?.error {
                        newNotice = error
                    } else if newRunning {
                        newNotice = "正在分析 \(status.job?.completed ?? 0)/\(status.job?.total ?? 0) 个编译单元…"
                    } else if wasRunning {
                        newNotice = status.job?.failures?.first?.error ?? (status.job?.state == "cancelled" ? "分析已取消。" : "语义索引已更新。")
                    } else {
                        newNotice = semanticNotice
                    }
                    if semanticNotice != newNotice {
                        semanticNotice = newNotice
                    }
                    if !semanticRunning, semanticWatchSettingsPending { await applySemanticWatchSettings() }
                    if changed || (wasRunning && !semanticRunning) {
                        semanticQueryRevision += 1
                        semanticTask?.cancel(); semanticContextValue = nil
                        semanticIncoming = []; semanticOutgoing = []; semanticDefinitions = []
                        await reloadExternalSemanticDocument()
                        queueSemanticContext()
                    }
                } catch {
                    if generation == semanticGeneration, semanticNotice != error.localizedDescription {
                        semanticNotice = error.localizedDescription
                    }
                }
                let delayMs = isIdle ? 2500 : 750
                try? await Task.sleep(for: .milliseconds(delayMs))
            }
        }
    }
    private func reloadExternalSemanticDocument() async {
        guard let previous = document else { return }
        let anchor = SourceCoordinates(previous.content).anchor(byte: caretByte)
        do {
            let disk = try await core.readDocument(path: previous.path)
            guard document?.path == previous.path, disk.content != previous.content else { return }
            await readFile(previous.path)
            guard let current = document, current.path == previous.path else { return }
            let byte = SourceCoordinates(current.content).byteOffset(line: anchor.line, column: anchor.column)
            moveCaret(path: current.path, byte: byte)
            navigate(to: AnalysisRange(startByte: byte, endByte: byte, startLine: anchor.line + 1, endLine: anchor.line + 1))
        } catch { semanticNotice = "当前文件已变化，无法重新读取：\(error.localizedDescription)" }
    }
    func analyzeCurrentSemantics(refreshIndexed: Bool = false) {
        guard semanticConfigured, !semanticRunning, let path = document?.path else { return }
        let generation = semanticGeneration
        semanticRunning = true; semanticNotice = "正在分析；语法阅读仍可继续。"
        Task {
            do {
                if refreshIndexed { try await core.refreshIndexedSemantics() }
                else { try await core.indexSemantics(path: path) }
            }
            catch { if generation == semanticGeneration { semanticRunning = false; semanticNotice = error.localizedDescription } }
        }
    }
    func cancelSemanticAnalysis() { Task { try? await core.cancelSemantics() } }
    func revealSemanticDefinition(_ entity: String) {
        let generation = semanticGeneration
        Task {
            do {
                let result = try await core.semanticDefinitions(entity: entity)
                guard generation == semanticGeneration else { return }
                var seen = Set<String>()
                let unique = result.rows.filter { seen.insert("\($0.location.path):\($0.location.offset)").inserted }
                semanticDefinitions = unique
                if unique.count == 1, let symbol = unique.first { openSemanticLocation(symbol.location) }
                else if unique.isEmpty { semanticNotice = "目标已绑定到声明，实现位于外部或尚未索引。" }
                else { semanticNotice = "存在多个定义/编译变体，请选择目标。" }
            } catch { semanticNotice = error.localizedDescription }
        }
    }
}
