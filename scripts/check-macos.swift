// Compile against production files; works with Command Line Tools, without XCTest.
import AppKit
import Foundation

@main
enum MacChecks {
    enum Failure: Error { case expectation(String) }
    static func expect(_ value: Bool, _ message: String) throws {
        if !value { throw Failure.expectation(message) }
    }

    @MainActor
    static func main() async throws {
        let tree = FileNode.build(from: [
            FileEntry(path: "b/main.rs", name: "main.rs", kind: "file", language: "Rust", sizeBytes: 8),
            FileEntry(path: "a/main.rs", name: "main.rs", kind: "file", language: "Rust", sizeBytes: 8),
            FileEntry(path: "README.md", name: "README.md", kind: "file", language: "Markdown", sizeBytes: 2),
            FileEntry(path: "b", name: "b", kind: "directory", language: nil, sizeBytes: 0),
            FileEntry(path: "a", name: "a", kind: "directory", language: nil, sizeBytes: 0),
        ])
        try expect(tree.map(\.id) == ["a", "b", "README.md"], "Folders must sort first.")
        try expect(tree[0].children?.first?.id == "a/main.rs", "File identity must include its path.")
        try expect(tree[1].children?.first?.id == "b/main.rs", "Duplicate basenames must not collide.")
        print("PASS file tree identity")

        try expect(AppearanceChoice.system.nsAppearance == nil, "System mode must clear the appearance override.")
        let background = SourcePalette.background
        var light = 0.0
        var dark = 0.0
        AppearanceChoice.light.nsAppearance!.performAsCurrentDrawingAppearance {
            light = background.usingColorSpace(.sRGB)!.redComponent
        }
        AppearanceChoice.dark.nsAppearance!.performAsCurrentDrawingAppearance {
            dark = background.usingColorSpace(.sRGB)!.redComponent
        }
        try expect(light > 0.95 && dark > 0.13 && dark < 0.25, "One retained color must resolve to both lighter surfaces.")
        print("PASS system appearance and dynamic palette")

        let root = FileManager.default.temporaryDirectory.appendingPathComponent("symweft-check-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root.appendingPathComponent("target/debug"), withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let content = "print('织源')\nservice.first().second()\n"
        try content.write(to: root.appendingPathComponent("你好.py"), atomically: true, encoding: .utf8)
        try "hidden".write(to: root.appendingPathComponent("target/debug/hidden.rs"), atomically: true, encoding: .utf8)
        let core = CoreClient()
        do {
            let info = try await core.connect()
            try expect(info.protocolVersion == 1 && info.analysisLevel == "syntax_on_demand" && info.capabilities.contains("document.edit"), "Handshake must declare actual capabilities.")
            let workspace = try await core.openWorkspace(at: root)
            try expect(workspace.fileCount == 1 && workspace.languages["Python"] == 1, "Rust scan must exclude build output.")
            let document = try await core.readDocument(path: "你好.py")
            try expect(document.content == content && document.sizeBytes == UInt64(content.utf8.count), "Unicode and byte lengths must survive the pipe.")
            print("PASS Rust/Swift handshake, scan and UTF-8 document")
            let opened = try await core.openDocument(path: "你好.py")
            try expect(opened.analysis?.facts.calls.first?.callee == "print", "Real grammar facts must reach Swift.")
            try expect(opened.analysis?.facts.calls.first?.resolution == "unresolved", "Syntax must not claim a resolved edge.")
            let calls = opened.analysis!.facts.calls
            try expect(calls.count == 3 && Set(calls.map(\.id)).count == calls.count, "Chained calls sharing a start byte must keep distinct UI identities.")
            let revision = opened.analysis!.revision
            let edited = try await core.editDocument(path: "你好.py", revision: revision, startByte: 0, oldEndByte: 5, newText: "other")
            try expect(edited.update.mode == "incremental" && edited.analysis.facts.calls.first?.callee == "other", "Edits must update the shared analysis.")
            do {
                _ = try await core.context(path: "你好.py", revision: revision, byteOffset: 0)
                throw Failure.expectation("Old revisions must be rejected.")
            } catch let error as CoreFault {
                try expect(error.code == "stale_revision", "Expected a revision conflict.")
            }
            let context = try await core.context(path: "你好.py", revision: edited.analysis.revision, byteOffset: 1)
            try expect(context.call?.callee == "other", "Hot context lookup must use the updated snapshot.")
            let diskContent = try String(contentsOf: root.appendingPathComponent("你好.py"), encoding: .utf8)
            try expect(diskContent == content, "Engine edits must not write to disk.")
            print("PASS real syntax, incremental edit, revision conflict and context query")
            do {
                _ = try await core.readDocument(path: "../outside")
                throw Failure.expectation("Traversal should fail.")
            } catch let error as CoreFault {
                try expect(error.code == "invalid_path", "Expected a structured core error.")
            }
            let recovered = try await core.readDocument(path: "你好.py")
            try expect(recovered.content == content, "Business errors must preserve the connection and workspace.")
            print("PASS protocol error recovery")
            try await core.flushIndex()
            await core.stop()
            _ = try await core.connect()
            _ = try await core.openWorkspace(at: root)
            let settings = try await core.configureCache(mode: "manual", maxBytes: 8 * 1024 * 1024, prefetch: false)
            try expect(settings.settings.mode == "manual", "Manual cache configuration must reach Rust.")
            let restored = try await core.openDocument(path: "你好.py")
            try expect(restored.storage?.source == "sqlite" && restored.update?.mode == "restored", "Restart must restore persisted facts without parsing.")
            try expect(restored.analysis?.facts.calls.first?.callee == "print", "Unsaved overlay facts must never replace disk facts.")
            try await core.pressure(level: "critical", busy: true)
            let constrained = try await core.cacheStatus()
            try expect(constrained.cache.effectiveBytes <= 1024 * 1024, "Critical pressure must shrink the fact budget.")
            let cSource = "int target(int x){return x+1;}\n"
            let cppSource = "extern \"C\" int target(int);\nint bridge(){return target(1);}\n"
            try cSource.write(to: root.appendingPathComponent("a.c"), atomically: true, encoding: .utf8)
            try cppSource.write(to: root.appendingPathComponent("b.cpp"), atomically: true, encoding: .utf8)
            let commands: [[String: Any]] = [
                ["directory": root.path, "file": "a.c", "arguments": ["clang", "-std=c11", "-c", "a.c"]],
                ["directory": root.path, "file": "b.cpp", "arguments": ["clang++", "-std=c++17", "-c", "b.cpp"]],
            ]
            let commandsURL = root.appendingPathComponent("compile_commands.json")
            try JSONSerialization.data(withJSONObject: commands).write(to: commandsURL)
            let configured = try await core.configureSemantics(path: commandsURL.path)
            try expect(configured.configuredUnits == 2, "Swift must decode the semantic configuration.")
            for path in ["a.c", "b.cpp"] {
                try await core.indexSemantics(path: path)
                let deadline = ProcessInfo.processInfo.systemUptime + 5
                var progress = try await core.semanticProgress()
                while progress.job?.state == "running", ProcessInfo.processInfo.systemUptime < deadline {
                    try await Task.sleep(for: .milliseconds(20))
                    progress = try await core.semanticProgress()
                }
                try expect(progress.job?.state == "complete", "Swift indexing and progress must complete.")
            }
            let cppOpened = try await core.openDocument(path: "b.cpp")
            let name = cppSource.range(of: "target(1)")!
            let byteOffset = cppSource[..<name.lowerBound].utf8.count
            let semantic = try await core.semanticContext(path: "b.cpp", byte: byteOffset)
            try expect(semantic.enclosingFunctions.count == 1 && semantic.enclosingFunctions[0].unitId != nil, "Semantic function and TU ownership must reach Swift.")
            let outgoing = try await core.semanticCalls(entity: semantic.enclosingFunctions[0].id, direction: "outgoing")
            try expect(outgoing.rows.count == 1 && outgoing.rows[0].target == semantic.entityId, "Swift must preserve the compiler-bound target.")
            let definitions = try await core.semanticDefinitions(entity: semantic.entityId!)
            try expect(definitions.rows.count == 1 && definitions.rows[0].location.path.hasSuffix("/a.c"), "Swift must navigate to the actual cross-file definition.")
            print("PASS Swift semantic configuration, async indexing, DTOs and cross-file definition")
            let live = try await core.watchSemantics(autoRefresh: false, strict: false)
            try expect(live.enabled && !live.autoRefresh && live.metadataCache, "Native watch settings must reach Rust and decode in Swift.")
            try expect(try await core.semanticProgress().watch != nil, "Progress must include native watch state.")
            let watchedMetrics = try await core.semanticProgress().watch
            try expect(watchedMetrics?.fingerprints != nil && watchedMetrics?.configurationFingerprints != nil && watchedMetrics?.autoJobs != nil,
                "Native performance counters must decode without changing the watch contract.")
            // Edit only in the protocol buffer, then await the disposable worker
            // through the same DTOs the future native editor will consume.
            let cppRevision = cppOpened.analysis!.revision
            let editedCPP = try await core.editDocument(path: "b.cpp", revision: cppRevision,
                startByte: UInt64(byteOffset + 7), oldEndByte: UInt64(byteOffset + 8), newText: "2")
            let preview = try await core.startSemanticPreview(path: "b.cpp", revision: editedCPP.analysis.revision)
            let ticket = preview.ticket!
            let previewDeadline = ProcessInfo.processInfo.systemUptime + 5
            var previewState = try await core.semanticPreviewProgress(path: "b.cpp", ticket: ticket)
            while ["queued", "running"].contains(previewState.state), ProcessInfo.processInfo.systemUptime < previewDeadline {
                _ = try await core.readDocument(path: "a.c")
                try await Task.sleep(for: .milliseconds(20))
                previewState = try await core.semanticPreviewProgress(path: "b.cpp", ticket: ticket)
            }
            try expect(previewState.state == "ready" && previewState.chargedBytes > 0, "Swift async preview must retain a bounded result.")
            try expect(previewState.queryVersion != nil, "Preview must expose single-TU query_version.")
            try expect(previewState.projectQueryVersion != nil, "Preview must expose project_query_version.")
            try expect(previewState.completeness != nil, "Preview must expose dynamic completeness.")

            // 1. Legacy single-TU preview context and relations query (backward compatibility check)
            let previewContext = try await core.semanticPreviewContext(path: "b.cpp", ticket: ticket, byte: byteOffset)
            let previewCalls = try await core.semanticPreviewCalls(path: "b.cpp", ticket: ticket, entity: previewContext.enclosingFunctions[0].id, direction: "outgoing")
            try expect(previewCalls.rows.first?.target == semantic.entityId, "Overlay query must preserve the compiler target.")
            try expect(try String(contentsOf: root.appendingPathComponent("b.cpp"), encoding: .utf8) == cppSource, "Preview must not save the buffer.")

            // 2. Project-level context and relations query through CoreClient (OVL-12)
            let projectQueryVersion = previewState.projectQueryVersion!
            let projContext = try await core.semanticProjectContext(path: "b.cpp", ticket: ticket, byte: byteOffset, queryVersion: projectQueryVersion)
            try expect(projContext.entityId == semantic.entityId, "Project context must resolve the entity.")
            try expect(projContext.completeness != nil, "Project context must report completeness.")

            let projCalls = try await core.semanticProjectCalls(path: "b.cpp", ticket: ticket, entity: previewContext.enclosingFunctions[0].id, direction: "outgoing", queryVersion: projectQueryVersion)
            try expect(projCalls.rows.count == 1 && projCalls.rows[0].target == semantic.entityId, "Project relations must preserve compiler target.")

            // Project query merges definitions from other TU (a.c) whereas legacy preview does not
            let projDefs = try await core.semanticProjectDefinitions(path: "b.cpp", ticket: ticket, entity: semantic.entityId!, queryVersion: projectQueryVersion)
            try expect(projDefs.rows.count == 1 && projDefs.rows[0].location.path.hasSuffix("/a.c"), "Project relations must merge definition from a.c.")
            try expect(projDefs.completeness != nil, "Project rows must report completeness.")

            // 3. Cancel preview and verify that old ticket is strictly rejected (cannot be misused)
            let cancelledPreview = try await core.cancelSemanticPreview(ticket: ticket)
            try expect(cancelledPreview.state == "cancelled" && cancelledPreview.chargedBytes == 0, "Swift cancellation must release preview facts.")
            do {
                _ = try await core.semanticProjectCalls(path: "b.cpp", ticket: ticket, entity: previewContext.enclosingFunctions[0].id, direction: "outgoing", queryVersion: projectQueryVersion)
                throw Failure.expectation("Cancelled or obsolete tickets must be rejected.")
            } catch let fault as CoreFault {
                try expect(["invalid_params", "preview_not_ready", "index_error"].contains(fault.code), "Old cancelled ticket must fail with clear fault code.")
            }
            do {
                _ = try await core.semanticPreviewContext(path: "b.cpp", ticket: ticket, byte: byteOffset)
                throw Failure.expectation("Cancelled tickets must be rejected in legacy preview.")
            } catch let fault as CoreFault {
                try expect(["invalid_params", "preview_not_ready", "index_error"].contains(fault.code), "Cancelled ticket must be rejected.")
            }
            print("PASS Swift async semantic preview, project merge, DTO decoding, pagination and old ticket rejection")
            try await core.flushIndex()
            await core.stop()
            print("PASS persistent restart, overlay isolation, manual cache and memory pressure")
        } catch {
            await core.stop()
            throw error
        }
        let coordinateText = "a😀织源\r\ne\u{301}()\nlast"
        let coordinates = SourceCoordinates(coordinateText)
        var byte = 0, unit = 0
        for scalar in coordinateText.unicodeScalars {
            try expect(coordinates.byteOffset(utf16: unit) == byte, "UTF-16 to UTF-8 scalar boundary mismatch.")
            try expect(coordinates.utf16Offset(byte: byte) == unit, "UTF-8 to UTF-16 scalar boundary mismatch.")
            byte += scalar.utf8.count
            unit += scalar.value > 0xFFFF ? 2 : 1
        }
        try expect(coordinates.byteOffset(utf16: unit) == byte, "EOF coordinates must round-trip.")
        try expect(coordinates.byteOffset(utf16: 2) == nil, "A half surrogate must not become a valid byte position.")
        try expect(coordinates.utf16Offset(byte: 2) == nil, "A split UTF-8 scalar must be rejected.")
        let anchor = coordinates.anchor(byte: 6)
        try expect(coordinates.byteOffset(line: anchor.line, column: anchor.column) == 5, "Reload anchors must round to a valid UTF-8 boundary.")
        print("PASS caret coordinates with emoji, combining marks and CRLF")
        print("All macOS foundation checks passed.")
    }
}
