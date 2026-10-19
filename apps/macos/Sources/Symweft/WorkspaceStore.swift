import AppKit
import SwiftUI
import Darwin

@MainActor
final class WorkspaceStore: ObservableObject {
    @Published private(set) var workspace: WorkspaceSnapshot?
    @Published private(set) var fileTree: [FileNode] = []
    @Published private(set) var treeGeneration = 0
    @Published private(set) var document: SourceDocument?
    @Published private(set) var analysis: DocumentAnalysis?
    @Published private(set) var analysisUpdate: AnalysisUpdate?
    @Published private(set) var analysisNote: String?
    @Published private(set) var caretByte = 0
    @Published private(set) var sourceNavigation: SourceNavigation?
    @Published private(set) var coreInfo: CoreInfo?
    @Published private(set) var isOpening = false
    @Published private(set) var isReading = false
    @Published private(set) var selectedPath: String?
    @Published var errorMessage: String?
    @Published var showInspector = true
    @Published private(set) var cacheStatus: CacheStatus?
    @Published private(set) var storageInfo: StorageInfo?
    @Published var cacheMode = UserDefaults.standard.string(forKey: "cacheMode") ?? "auto" { didSet { queueCacheSettings() } }
    @Published var cacheMiB = max(8, UserDefaults.standard.integer(forKey: "cacheMiB") == 0 ? 64 : UserDefaults.standard.integer(forKey: "cacheMiB")) { didSet { queueCacheSettings() } }
    @Published var prefetchEnabled = UserDefaults.standard.object(forKey: "prefetchEnabled") as? Bool ?? true { didSet { queueCacheSettings() } }
    @Published var collectAttention = UserDefaults.standard.object(forKey: "collectAttention") as? Bool ?? true { didSet { UserDefaults.standard.set(collectAttention, forKey: "collectAttention") } }
    @Published var semanticConfigured = false
    @Published var semanticRunning = false
    @Published var semanticNotice: String?
    @Published var semanticContextValue: SemanticContext?
    @Published var semanticIncoming: [SemanticCall] = []
    @Published var semanticOutgoing: [SemanticCall] = []
    @Published var semanticDefinitions: [SemanticSymbol] = []
    @Published var semanticUnitID: String?
    @Published var semanticWatch: SemanticWatch?
    @Published var semanticAutoRefresh = true
    @Published var semanticStrictValidation = false
    var semanticWatchSettingsPending = false
    var semanticQueryRevision = 0
    var semanticTask: Task<Void, Never>?
    var semanticPoll: Task<Void, Never>?
    var semanticGeneration = 0
    let core = CoreClient()
    private var fileTask: Task<Void, Never>?
    private var navigationToken = 0
    private var settingsTask: Task<Void, Never>?
    private var metricsTask: Task<Void, Never>?
    private var pressureSource: DispatchSourceMemoryPressure?
    private var eventMonitor: Any?
    private var pressureLevel = "normal"
    private var lastInteraction: TimeInterval = 0
    private var lastMetrics: TimeInterval = ProcessInfo.processInfo.systemUptime

    private func queueCacheSettings() {
        guard !LaunchOptions.isDiagnostic else { return }
        UserDefaults.standard.set(cacheMode, forKey: "cacheMode")
        UserDefaults.standard.set(cacheMiB, forKey: "cacheMiB")
        UserDefaults.standard.set(prefetchEnabled, forKey: "prefetchEnabled")
        settingsTask?.cancel()
        settingsTask = Task { try? await Task.sleep(for: .milliseconds(250)); guard !Task.isCancelled else { return }; await applyCacheSettings() }
    }
    private func applyCacheSettings() async {
        guard workspace != nil else { return }
        do { cacheStatus = try await core.configureCache(mode: cacheMode, maxBytes: UInt64(cacheMiB.clamped(to: 8...1024)) * 1024 * 1024, prefetch: prefetchEnabled) }
        catch { /* Reading remains usable when persistence is unavailable. */ }
    }
    func clearAttention() {
        Task {
            do { try await core.clearUsage() }
            catch { errorMessage = error.localizedDescription }
        }
    }
    private func startMetrics() {
        guard metricsTask == nil, !LaunchOptions.isCapturing else { return }
        let source = DispatchSource.makeMemoryPressureSource(eventMask: [.normal, .warning, .critical], queue: .main)
        pressureSource = source
        source.setEventHandler { [weak self] in
            guard let data = self?.pressureSource?.data else { return }
            Task { @MainActor in self?.pressureLevel = data.contains(.critical) ? "critical" : data.contains(.warning) ? "warning" : "normal" }
        }
        source.resume()
        eventMonitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .leftMouseDown, .scrollWheel]) { [weak self] event in
            Task { @MainActor in self?.lastInteraction = ProcessInfo.processInfo.systemUptime }
            return event
        }
        metricsTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(5))
                guard !Task.isCancelled, let self else { return }
                let now = ProcessInfo.processInfo.systemUptime
                let elapsed = UInt64(max(0, min(5, now - self.lastMetrics)) * 1000)
                self.lastMetrics = now
                guard self.workspace != nil else { continue }
                var load = 0.0
                let busy = getloadavg(&load, 1) == 1 && load > Double(ProcessInfo.processInfo.activeProcessorCount) * 0.8
                try? await self.core.pressure(level: self.pressureLevel, busy: busy)
                if self.collectAttention, !LaunchOptions.isProfiling, NSApp.isActive, now - self.lastInteraction < 30,
                   NSApp.windows.contains(where: { $0.isVisible && $0.occlusionState.contains(.visible) && $0.firstResponder is SourceTextView }),
                   let analysis = self.analysis {
                    try? await self.core.recordUsage(path: analysis.path, revision: analysis.revision, localID: self.currentSymbol?.localId, activeMS: elapsed, opened: false)
                }
                let newStatus = try? await self.core.cacheStatus()
                if self.cacheStatus != newStatus {
                    self.cacheStatus = newStatus
                }
            }
        }
    }

    var currentFunction: AnalysisSymbol? {
        analysis?.facts.symbols.filter { $0.isCallable && $0.range.contains(caretByte) }
            .min { $0.range.endByte - $0.range.startByte < $1.range.endByte - $1.range.startByte }
    }
    var currentSymbol: AnalysisSymbol? {
        analysis?.facts.symbols.filter { $0.range.contains(caretByte) }
            .min { $0.range.endByte - $0.range.startByte < $1.range.endByte - $1.range.startByte }
    }
    var currentCalls: [AnalysisCall] {
        let owner = currentFunction?.localId
        return analysis?.facts.calls.filter { $0.ownerId == owner } ?? []
    }

    func moveCaret(path: String, byte: Int) {
        guard document?.path == path, caretByte != byte else { return }
        caretByte = byte
        queueSemanticContext()
    }

    func navigate(to range: AnalysisRange) {
        guard let document else { return }
        navigationToken += 1
        sourceNavigation = SourceNavigation(token: navigationToken, path: document.path, range: range)
    }

    func bootstrap() async {
        do { coreInfo = try await core.connect() }
        catch { errorMessage = error.localizedDescription }
        startMetrics()
        if let path = LaunchOptions.value(after: "--workspace") {
            await openWorkspace(URL(fileURLWithPath: path))
        }
        if let path = LaunchOptions.value(after: "--compile-commands") {
            await configureSemantics(URL(fileURLWithPath: path))
        }
    }

    func chooseWorkspace() {
        let panel = NSOpenPanel()
        panel.title = "打开工作空间"
        panel.message = "选择本地项目目录。Symweft 仅扫描文件，不运行构建脚本。"
        panel.prompt = "打开"
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.begin { [weak self] response in
            guard response == .OK, let url = panel.url else { return }
            Task { @MainActor in await self?.openWorkspace(url) }
        }
    }

    func openWorkspace(_ url: URL) async {
        guard !isOpening else { return }
        resetSemantics()
        fileTask?.cancel()
        isReading = false
        isOpening = true
        errorMessage = nil
        defer { isOpening = false }
        do {
            let snapshot = try await core.openWorkspace(at: url)
            workspace = snapshot
            await applyCacheSettings()
            fileTree = FileNode.build(from: snapshot.entries)
            treeGeneration += 1
            document = nil
            analysis = nil
            analysisUpdate = nil
            analysisNote = nil
            sourceNavigation = nil
            let first = snapshot.entries.first { $0.path == LaunchOptions.value(after: "--file") }
                ?? snapshot.entries.first { $0.path == "README.md" }
                ?? snapshot.entries.first { !$0.isDirectory && $0.language != nil }
            selectedPath = first?.path
            if let path = first?.path { await readFile(path) }
        } catch { errorMessage = error.localizedDescription }
    }

    func refresh() {
        guard let workspace else { return }
        Task { await openWorkspace(URL(fileURLWithPath: workspace.root)) }
    }

    func selectFile(_ path: String?) {
        selectedPath = path
        if !LaunchOptions.isDiagnostic { lastInteraction = ProcessInfo.processInfo.systemUptime }
        guard let path, workspace?.entries.first(where: { $0.path == path })?.isDirectory == false else { return }
        guard document?.path != path else { return }
        fileTask?.cancel()
        fileTask = Task {
            await readFile(path)
            if !Task.isCancelled, collectAttention, !LaunchOptions.isDiagnostic, let analysis, analysis.path == path {
                try? await core.recordUsage(path: path, revision: analysis.revision, localID: nil, activeMS: 0, opened: true)
            }
        }
    }

    func readFile(_ path: String) async {
        isReading = true
        defer { if !Task.isCancelled { isReading = false } }
        do {
            let next = try await core.openDocument(path: path)
            guard !Task.isCancelled, selectedPath == path else { return }
            document = next.document
            analysis = next.analysis
            analysisUpdate = next.update
            storageInfo = next.storage
            caretByte = 0
            sourceNavigation = nil
            semanticUnitID = nil
            queueSemanticContext()
            if let error = next.analysisError {
                switch error.code {
                case "unsupported_language": analysisNote = "此语言目前仅提供文本阅读。"
                case "analysis_budget", "analysis_timeout": analysisNote = "已达到交互分析预算，保留文本阅读。"
                default: analysisNote = "分析暂不可用：\(error.code)"
                }
            } else {
                analysisNote = next.analysis?.facts.hasSyntaxErrors == true ? "文件含语法错误，结构可能不完整。" : nil
            }
        } catch {
            guard !Task.isCancelled, selectedPath == path else { return }
            document = nil
            analysis = nil
            analysisUpdate = nil
            analysisNote = nil
            errorMessage = error.localizedDescription
        }
    }

    func reveal(_ path: String) {
        guard let workspace else { return }
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: workspace.root).appendingPathComponent(path)])
    }
    func openSemanticLocation(_ location: SemanticLocation) {
        guard let workspace else { return }
        let root = URL(fileURLWithPath: workspace.root).resolvingSymlinksInPath().path + "/"
        let target = URL(fileURLWithPath: location.path).resolvingSymlinksInPath().path
        guard target.hasPrefix(root) else { semanticNotice = "目标位于工作空间外：\(location.path)"; return }
        let relative = String(target.dropFirst(root.count))
        fileTask?.cancel()
        selectedPath = relative
        fileTask = Task {
            if document?.path != relative { await readFile(relative) }
            guard !Task.isCancelled, document?.path == relative else { return }
            navigate(to: AnalysisRange(startByte: location.offset, endByte: location.endOffset, startLine: location.line, endLine: location.line))
        }
    }

    /// Opt-in native idle probe. Normal 750 ms semantic and 5 s resource timers
    /// stay active; synthetic runs never record attention or change preferences.
    func profileIdleIfRequested() async {
        guard let output = LaunchOptions.value(after: "--perf-idle-to") else { return }
        do {
            guard !LaunchOptions.isCapturing, errorMessage == nil, semanticConfigured,
                  document != nil else { throw CaptureError.noWindow }
            _ = try await core.configureCache(mode: "auto", maxBytes: 64 * 1024 * 1024, prefetch: true)
            if let name = LaunchOptions.value(after: "--inspect-symbol"),
               let symbol = analysis?.facts.symbols.first(where: { $0.name == name }) { navigate(to: symbol.nameRange) }
            var activity: [String: Any] = [:]
            if LaunchOptions.value(after: "--perf-refresh-indexed") == "true" {
                LaunchOptions.profileEvent("analysis_start")
                let started = Date().timeIntervalSince1970
                try await core.refreshIndexedSemantics()
                let clock = ContinuousClock()
                let deadline = clock.now.advanced(by: .seconds(45))
                var status = try await core.semanticProgress()
                while status.job?.state == "running", clock.now < deadline {
                    try await Task.sleep(for: .milliseconds(75))
                    status = try await core.semanticProgress()
                }
                guard status.job?.state == "complete" else { throw CaptureError.noWindow }
                LaunchOptions.profileEvent("analysis_complete")
                activity = ["start_unix_ns":UInt64(started * 1e9),"end_unix_ns":UInt64(Date().timeIntervalSince1970 * 1e9),
                    "analyzed_units":status.job?.analyzed ?? 0,"total_units":status.job?.total ?? 0,
                    "job_wall_ms":status.job?.wallMs ?? 0,"scope":"refresh previously indexed units only"]
            }
            try await Task.sleep(for: .seconds(1))
            await semanticTask?.value
            try await core.flushIndex()
            let before = try await core.semanticProgress()
            let seconds = min(30, max(6, Double(LaunchOptions.value(after: "--perf-idle-seconds") ?? "8") ?? 8))
            let start = Date().timeIntervalSince1970
            LaunchOptions.profileEvent("idle_start")
            if let ready = LaunchOptions.value(after: "--qa-ready-to") {
                try Data("idle\n".utf8).write(to: URL(fileURLWithPath: ready))
            }
            let clock = ContinuousClock()
            let begin = clock.now
            try await Task.sleep(for: .seconds(seconds))
            let end = Date().timeIntervalSince1970
            let span = begin.duration(to: clock.now)
            let after = try await core.semanticProgress()
            LaunchOptions.profileEvent("idle_complete")
            let counters = { (value: SemanticProgress) -> [String: UInt64] in
                ["input_hashes":value.watch?.fingerprints?.hashes ?? 0,
                 "input_bytes_read":value.watch?.fingerprints?.bytesRead ?? 0,
                 "configuration_hashes":value.watch?.configurationFingerprints?.hashes ?? 0,
                 "auto_jobs":value.watch?.autoJobs ?? 0]
            }
            let report: [String: Any] = ["status":"ok","ui_pid":ProcessInfo.processInfo.processIdentifier,
                "start_unix_ns":UInt64(start * 1e9),"end_unix_ns":UInt64(end * 1e9),
                "idle_seconds":Double(span.components.seconds) + Double(span.components.attoseconds) / 1e18,
                "before":counters(before),"after":counters(after),
                "normal_metrics_timer":true,"attention_recording":false,
                "activity":activity,
                "ui_active":NSApp.isActive,
                "visible_windows":NSApp.windows.filter { $0.isVisible }.count,
                "unoccluded_windows":NSApp.windows.filter { $0.isVisible && $0.occlusionState.contains(.visible) }.count,
                "semantic_configured":semanticConfigured,"current_function":currentFunction?.name ?? "none",
                "semantic_outgoing":semanticOutgoing.count,"watch_enabled":after.watch?.enabled ?? false]
            try JSONSerialization.data(withJSONObject: report, options: [.prettyPrinted,.sortedKeys]).write(to: URL(fileURLWithPath: output))
            semanticPoll?.cancel(); semanticTask?.cancel(); metricsTask?.cancel()
            await core.stop()
            NSApp.terminate(nil)
        } catch {
            FileHandle.standardError.write(Data("Performance probe failed: \(error)\n".utf8))
            await core.stop()
            Darwin.exit(1)
        }
    }

    /// Local developer QA: render our own view, without capturing the user's desktop.
    func captureIfRequested() async {
        guard let path = LaunchOptions.value(after: "--capture-to") else { return }
        do {
            if let name = LaunchOptions.value(after: "--inspect-symbol"),
               let symbol = analysis?.facts.symbols.first(where: { $0.name == name }) {
                navigate(to: symbol.nameRange)
            }
            NSApp.activate(ignoringOtherApps: true)
            NSApp.windows.first(where: { $0.isVisible })?.makeKeyAndOrderFront(nil)
            if let ready = LaunchOptions.value(after: "--qa-ready-to") {
                try Data("ready\n".utf8).write(to: URL(fileURLWithPath: ready))
            }
            let captureDelay = min(15_000, max(700, Int(LaunchOptions.value(after: "--capture-delay-ms") ?? "700") ?? 700))
            try await Task.sleep(for: .milliseconds(captureDelay))
            // Wait for the same debounced request used by interactive navigation.
            // A fixed frame delay alone can capture before a large file's query.
            await semanticTask?.value
            guard let window = NSApp.windows.first(where: { $0.contentView != nil && $0.isVisible }),
                  let content = window.contentView
            else { throw CaptureError.noWindow }
            let view = content.superview ?? content
            view.layoutSubtreeIfNeeded()
            window.displayIfNeeded()
            let scale = window.backingScaleFactor
            guard let bitmap = NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: Int(view.bounds.width * scale),
                pixelsHigh: Int(view.bounds.height * scale), bitsPerSample: 8, samplesPerPixel: 4,
                hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
            ), let graphics = NSGraphicsContext(bitmapImageRep: bitmap), let layer = view.layer
            else { throw CaptureError.noWindow }
            graphics.cgContext.scaleBy(x: scale, y: scale)
            layer.render(in: graphics.cgContext)
            guard let png = bitmap.representation(using: .png, properties: [:]) else { throw CaptureError.noWindow }
            try png.write(to: URL(fileURLWithPath: path))
            let report: [String: Any] = [
                "status": errorMessage == nil ? "ok" : "error",
                "workspace": workspace?.name ?? "none",
                "file_count": workspace?.fileCount ?? 0,
                "document": document?.path ?? "none",
                "analysis_level": analysis?.level ?? "text_only",
                "analysis_symbols": analysis?.facts.symbols.count ?? 0,
                "analysis_calls": analysis?.facts.calls.count ?? 0,
                "current_function": currentFunction?.name ?? "none",
                "current_scope_calls": currentCalls.count,
                "storage_source": storageInfo?.source ?? "none",
                "semantic_configured": semanticConfigured,
                "caret_byte": caretByte,
                "semantic_context_functions": semanticContextValue?.enclosingFunctions.count ?? 0,
                "semantic_notice": semanticNotice ?? "",
                "semantic_watch_revision": semanticWatch?.revision ?? 0,
                "semantic_outgoing_names": semanticOutgoing.prefix(10).map(\.name),
                "document_size_bytes": document?.sizeBytes ?? 0,
                "semantic_incoming": semanticIncoming.count,
                "semantic_outgoing": semanticOutgoing.count,
                "core_version": coreInfo?.version ?? "unavailable",
                "appearance": NSApp.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua])?.rawValue ?? "unknown",
                "error": errorMessage ?? "",
                "reader_geometry": readerGeometry(in: view),
                "navigator_geometry": navigatorGeometry(in: view),
            ]
            let data = try JSONSerialization.data(withJSONObject: report, options: [.sortedKeys])
            print(String(decoding: data, as: UTF8.self))
            try await core.flushIndex()
            await core.stop()
            if errorMessage != nil { Darwin.exit(1) }
            NSApp.terminate(nil)
        } catch {
            FileHandle.standardError.write(Data("Capture failed: \(error)\n".utf8))
            await core.stop()
            Darwin.exit(1)
        }
    }
    private enum CaptureError: Error { case noWindow }

    private func readerGeometry(in view: NSView) -> String {
        if let text = view as? SourceTextView {
            let origin = text.convert(NSPoint.zero, to: nil)
            let ruler = text.enclosingScrollView?.verticalRulerView
            let rulerRect = ruler?.convert(ruler?.bounds ?? .zero, to: nil)
            return "origin=\(origin); inset=\(text.textContainerOrigin); ruler=\(String(describing: rulerRect)); clip-frame=\(String(describing: text.enclosingScrollView?.contentView.frame)); clip-bounds=\(String(describing: text.enclosingScrollView?.contentView.bounds))"
        }
        return view.subviews.map { readerGeometry(in: $0) }.first { !$0.isEmpty } ?? ""
    }
    private func navigatorGeometry(in view: NSView) -> String {
        if let outline = view as? NSOutlineView {
            return "frame=\(outline.frame); rows=\(outline.numberOfRows); hidden=\(outline.isHiddenOrHasHiddenAncestor)"
        }
        return view.subviews.map { navigatorGeometry(in: $0) }.first { !$0.isEmpty } ?? ""
    }
}

private extension Int {
    func clamped(to range: ClosedRange<Int>) -> Int { Swift.min(range.upperBound, Swift.max(range.lowerBound, self)) }
}
