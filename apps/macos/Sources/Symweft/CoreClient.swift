import Foundation
import Darwin

/// Serialized, bounded pipe IO runs on this actor, away from the UI actor.
actor CoreClient {
    private var process: Process?
    private var input: FileHandle?
    private var output: FileHandle?
    private var buffered = Data()
    private var nextID: UInt64 = 0
    private let decoder: JSONDecoder = {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return decoder
    }()

    private struct Request: Encodable {
        let id: UInt64
        let method: String
        let params: [String: Parameter]
    }
    private struct AnyEncodable: Encodable {
        private let encodeFunc: (Encoder) throws -> Void
        init<T: Encodable>(_ wrapped: T) {
            encodeFunc = { try wrapped.encode(to: $0) }
        }
        func encode(to encoder: Encoder) throws {
            try encodeFunc(encoder)
        }
    }
    private enum Parameter: Encodable {
        case string(String), strings([String]), integer(UInt64), boolean(Bool), custom(AnyEncodable)
        func encode(to encoder: Encoder) throws {
            switch self {
            case .string(let string):
                var value = encoder.singleValueContainer()
                try value.encode(string)
            case .strings(let strings):
                var value = encoder.singleValueContainer()
                try value.encode(strings)
            case .integer(let integer):
                var value = encoder.singleValueContainer()
                try value.encode(integer)
            case .boolean(let boolean):
                var value = encoder.singleValueContainer()
                try value.encode(boolean)
            case .custom(let custom):
                try custom.encode(to: encoder)
            }
        }
    }
    private struct Envelope<T: Decodable>: Decodable {
        let id: UInt64?
        let protocolVersion: Int
        let result: T?
        let error: CoreFault?
    }

    enum TransportError: Error, LocalizedError {
        case unavailable(String), invalidReply, timeout, closed, oversized
        var errorDescription: String? {
            switch self {
            case .unavailable(let detail): "无法启动 Rust 核心。\(detail)"
            case .invalidReply: "核心协议版本或请求响应不匹配。"
            case .timeout: "核心响应超时。请重新打开工作空间后重试。"
            case .closed: "核心连接已关闭。请重新打开工作空间。"
            case .oversized: "核心响应超过当前传输大小限制。"
            }
        }
    }

    func connect() throws -> CoreInfo {
        try ensureRunning()
        return try exchange("initialize", params: [:])
    }
    func openWorkspace(at root: URL) throws -> WorkspaceSnapshot {
        try ensureRunning()
        return try exchange("workspace.open", params: ["root": .string(root.path)])
    }
    func readDocument(path: String) throws -> SourceDocument {
        try ensureRunning()
        return try exchange("document.read", params: ["path": .string(path)])
    }
    func openDocument(path: String) throws -> OpenDocument {
        try ensureRunning()
        return try exchange("document.open", params: ["path": .string(path)])
    }
    /// In-memory engine transaction; this does not save to disk.
    func editDocument(path: String, revision: UInt64, startByte: UInt64, oldEndByte: UInt64, newText: String) throws -> AnalysisResult {
        try ensureRunning()
        return try exchange("document.edit", params: ["path": .string(path), "base_revision": .integer(revision), "start_byte": .integer(startByte), "old_end_byte": .integer(oldEndByte), "new_text": .string(newText)])
    }
    func context(path: String, revision: UInt64, byteOffset: UInt64) throws -> AnalysisContext {
        try ensureRunning()
        return try exchange("analysis.at", params: ["path": .string(path), "revision": .integer(revision), "byte_offset": .integer(byteOffset)])
    }
    func configureCache(mode: String, maxBytes: UInt64, prefetch: Bool) throws -> CacheStatus {
        try ensureRunning()
        return try exchange("cache.configure", params: ["mode": .string(mode), "max_bytes": .integer(maxBytes), "prefetch": .boolean(prefetch)])
    }
    func cacheStatus() throws -> CacheStatus { try ensureRunning(); return try exchange("cache.status", params: [:]) }
    func pressure(level: String, busy: Bool) throws {
        let _: CoreAck = try exchange("cache.pressure", params: ["level": .string(level), "busy": .boolean(busy), "now_ms": .integer(UInt64(ProcessInfo.processInfo.systemUptime * 1000))])
    }
    func recordUsage(path: String, revision: UInt64, localID: Int?, activeMS: UInt64, opened: Bool) throws {
        var params: [String: Parameter] = ["path": .string(path), "revision": .integer(revision), "active_ms": .integer(activeMS), "opened": .boolean(opened)]
        if let localID { params["local_id"] = .integer(UInt64(localID)) }
        let _: CoreAck = try exchange("usage.record", params: params)
    }
    func flushIndex() throws { let _: CoreAck = try exchange("index.flush", params: [:]) }
    func clearUsage() throws { let _: CoreAck = try exchange("usage.clear", params: [:]) }
    func configureSemantics(path: String) throws -> SemanticProgress {
        try exchange("semantic.configure", params: ["compile_commands": .string(path)])
    }
    func indexSemantics(path: String) throws {
        // A single changed file is represented by the same array used by bulk clients.
        let _: CoreAck = try exchange("semantic.index", params: ["paths": .strings([path])])
    }
    func refreshIndexedSemantics() throws { let _: CoreAck = try exchange("semantic.refresh", params: [:]) }
    func semanticProgress() throws -> SemanticProgress { try exchange("semantic.progress", params: [:]) }
    func watchSemantics(autoRefresh: Bool, strict: Bool) throws -> SemanticWatch {
        try exchange("semantic.watch", params: ["enabled": .boolean(true), "auto_refresh": .boolean(autoRefresh), "metadata_cache": .boolean(!strict)])
    }
    func cancelSemantics() throws { let _: CoreAck = try exchange("semantic.cancel", params: [:]) }
    /// Starts work without waiting for libclang. The reader UI remains read-only;
    /// these methods also support the future revisioned editor integration.
    func startSemanticPreview(
        path: String,
        revision: UInt64,
        unitID: String? = nil,
        memoryMiB: UInt64 = 32,
        documents: [SemanticPreviewDocument]? = nil
    ) throws -> SemanticPreview {
        var params: [String: Parameter] = ["path": .string(path), "revision": .integer(revision), "memory_mib": .integer(memoryMiB)]
        if let unitID { params["unit_id"] = .string(unitID) }
        if let documents { params["documents"] = .custom(AnyEncodable(documents)) }
        return try exchange("semantic.preview.start", params: params)
    }
    private func previewParameters(path: String, ticket: SemanticPreviewTicket) -> [String: Parameter] {
        ["path": .string(path), "revision": .integer(ticket.revision), "job_id": .integer(ticket.jobId)]
    }
    func semanticPreviewProgress(path: String, ticket: SemanticPreviewTicket) throws -> SemanticPreview {
        try exchange("semantic.preview.progress", params: previewParameters(path: path, ticket: ticket))
    }
    func cancelSemanticPreview(ticket: SemanticPreviewTicket) throws -> SemanticPreview {
        try exchange("semantic.preview.cancel", params: ["job_id": .integer(ticket.jobId)])
    }
    /// Legacy single-TU context lookup (only queries current overlay TU facts).
    func semanticPreviewContext(path: String, ticket: SemanticPreviewTicket, byte: Int) throws -> SemanticContext {
        var params = previewParameters(path: path, ticket: ticket)
        params["byte_offset"] = .integer(UInt64(byte))
        return try exchange("semantic.preview.at", params: params)
    }
    /// Legacy single-TU relations query (only queries current overlay TU facts without merging other TUs).
    func semanticPreviewCalls(path: String, ticket: SemanticPreviewTicket, entity: String, direction: String) throws -> SemanticRows<SemanticCall> {
        var params = previewParameters(path: path, ticket: ticket)
        params["entity_id"] = .string(entity)
        params["direction"] = .string(direction)
        params["limit"] = .integer(100)
        return try exchange("semantic.preview.relations", params: params)
    }
    /// Project-level context query (merges active overlay with persistent index facts and secondary overlays).
    func semanticProjectContext(
        path: String,
        ticket: SemanticPreviewTicket,
        byte: Int,
        queryVersion: SemanticQueryVersion
    ) throws -> SemanticContext {
        var params = previewParameters(path: path, ticket: ticket)
        params["byte_offset"] = .integer(UInt64(byte))
        params["query_version"] = .custom(AnyEncodable(queryVersion))
        return try exchange("semantic.project.at", params: params)
    }
    /// Project-level relations query with pagination and stable query version.
    func semanticProjectCalls(
        path: String,
        ticket: SemanticPreviewTicket,
        entity: String,
        direction: String,
        limit: Int = 100,
        offset: Int = 0,
        queryVersion: SemanticQueryVersion
    ) throws -> SemanticRows<SemanticCall> {
        var params = previewParameters(path: path, ticket: ticket)
        params["entity_id"] = .string(entity)
        params["direction"] = .string(direction)
        params["limit"] = .integer(UInt64(limit))
        if offset > 0 { params["offset"] = .integer(UInt64(offset)) }
        params["query_version"] = .custom(AnyEncodable(queryVersion))
        return try exchange("semantic.project.relations", params: params)
    }
    /// Project-level definitions query with pagination and stable query version.
    func semanticProjectDefinitions(
        path: String,
        ticket: SemanticPreviewTicket,
        entity: String,
        limit: Int = 100,
        offset: Int = 0,
        queryVersion: SemanticQueryVersion
    ) throws -> SemanticRows<SemanticSymbol> {
        var params = previewParameters(path: path, ticket: ticket)
        params["entity_id"] = .string(entity)
        params["direction"] = .string("definitions")
        params["limit"] = .integer(UInt64(limit))
        if offset > 0 { params["offset"] = .integer(UInt64(offset)) }
        params["query_version"] = .custom(AnyEncodable(queryVersion))
        return try exchange("semantic.project.relations", params: params)
    }
    func semanticContext(path: String, byte: Int) throws -> SemanticContext {
        try exchange("semantic.at", params: ["path": .string(path), "byte_offset": .integer(UInt64(byte))])
    }
    func semanticCalls(entity: String, direction: String) throws -> SemanticRows<SemanticCall> {
        try exchange("semantic.relations", params: ["entity_id": .string(entity), "direction": .string(direction), "limit": .integer(100)])
    }
    func semanticDefinitions(entity: String) throws -> SemanticRows<SemanticSymbol> {
        try exchange("semantic.relations", params: ["entity_id": .string(entity), "direction": .string("definitions"), "limit": .integer(100)])
    }
    func stop() {
        try? input?.close()
        try? output?.close()
        if process?.isRunning == true { process?.terminate() }
        input = nil
        output = nil
        process = nil
        buffered.removeAll(keepingCapacity: false)
    }

    private func ensureRunning() throws {
        if process?.isRunning == true { return }
        stop()
        let executable: URL
        if let custom = ProcessInfo.processInfo.environment["SYMWEFT_CORE_PATH"] {
            executable = URL(fileURLWithPath: custom)
        } else {
            executable = Bundle.main.executableURL!.deletingLastPathComponent().appendingPathComponent("symweft-core")
        }
        guard FileManager.default.isExecutableFile(atPath: executable.path) else {
            throw TransportError.unavailable("请运行 scripts/build-macos.sh，或设置 SYMWEFT_CORE_PATH。")
        }
        let process = Process()
        let incoming = Pipe()
        let outgoing = Pipe()
        process.executableURL = executable
        process.arguments = ["serve"]
        process.standardInput = incoming
        process.standardOutput = outgoing
        process.standardError = FileHandle.standardError
        try process.run()
        self.process = process
        input = incoming.fileHandleForWriting
        output = outgoing.fileHandleForReading
        let info: CoreInfo = try exchange("initialize", params: [:])
        guard info.protocolVersion == 1 else { stop(); throw TransportError.invalidReply }
    }

    private func exchange<T: Decodable>(_ method: String, params: [String: Parameter]) throws -> T {
        guard let input, let output else { throw TransportError.closed }
        nextID += 1
        let requestID = nextID
        var data = try JSONEncoder().encode(Request(id: requestID, method: method, params: params))
        data.append(0x0A)
        do {
            try input.write(contentsOf: data)
            let deadline = ProcessInfo.processInfo.systemUptime + 30
            while true {
                if let newline = buffered.firstIndex(of: 0x0A) {
                    let line = Data(buffered[..<newline])
                    buffered.removeSubrange(...newline)
                    let envelope = try decoder.decode(Envelope<T>.self, from: line)
                    guard envelope.id == requestID, envelope.protocolVersion == 1 else { throw TransportError.invalidReply }
                    if let error = envelope.error { throw error }
                    guard let result = envelope.result else { throw TransportError.invalidReply }
                    return result
                }
                guard buffered.count < 16 * 1024 * 1024 else { throw TransportError.oversized }
                guard ProcessInfo.processInfo.systemUptime < deadline else { throw TransportError.timeout }
                var descriptor = pollfd(fd: output.fileDescriptor, events: Int16(POLLIN), revents: 0)
                let ready = poll(&descriptor, 1, 100)
                if ready < 0 {
                    if errno == EINTR { continue }
                    throw TransportError.closed
                }
                if ready == 0 { continue }
                // FileHandle may try to fill the requested buffer on a pipe.
                // One POSIX read consumes only bytes currently available after poll.
                var chunk = [UInt8](repeating: 0, count: 64 * 1024)
                let count = chunk.withUnsafeMutableBytes {
                    Darwin.read(output.fileDescriptor, $0.baseAddress!, $0.count)
                }
                if count < 0 {
                    if errno == EINTR || errno == EAGAIN { continue }
                    throw TransportError.closed
                }
                guard count > 0 else { throw TransportError.closed }
                buffered.append(contentsOf: chunk.prefix(count))
            }
        } catch let error as CoreFault {
            // A server error (e.g. binary text) must not discard the open workspace.
            throw error
        } catch {
            stop()
            throw error
        }
    }
}
