import Foundation

struct FileEntry: Decodable, Sendable {
    let path: String
    let name: String
    let kind: String
    let language: String?
    let sizeBytes: UInt64
    var isDirectory: Bool { kind == "directory" }
}

struct WorkspaceSnapshot: Decodable, Sendable {
    let root: String
    let name: String
    let entries: [FileEntry]
    let fileCount: Int
    let languages: [String: Int]
    let skippedEntries: Int
    let truncated: Bool
    let analysisLevel: String
}

struct SourceDocument: Decodable, Sendable {
    let path: String
    let language: String?
    let content: String
    let sizeBytes: UInt64
    let lineCount: Int
    let encoding: String
}

struct CoreInfo: Decodable, Sendable {
    let name: String
    let version: String
    let protocolVersion: Int
    let capabilities: [String]
    let analysisLevel: String
}

struct AnalysisRange: Decodable, Sendable {
    let startByte: Int
    let endByte: Int
    let startLine: Int
    let endLine: Int
    func contains(_ byte: Int) -> Bool { startByte <= byte && byte < endByte }
}

struct AnalysisSymbol: Decodable, Sendable, Identifiable {
    let localId: Int
    let name: String
    let kind: String
    let range: AnalysisRange
    let nameRange: AnalysisRange
    let parentId: Int?
    let isCallable: Bool
    var id: Int { localId }
}

struct AnalysisCall: Decodable, Sendable, Identifiable {
    let callee: String
    let kind: String
    let range: AnalysisRange
    let ownerId: Int?
    let evidence: String
    let resolution: String
    // Nested/chained calls often share a start byte; both bounds are required.
    var id: String { "\(range.startByte):\(range.endByte):\(kind)" }
    var displayName: String { callee.split(whereSeparator: { $0.isWhitespace }).joined(separator: " ") }
}

struct AnalysisFacts: Decodable, Sendable {
    let symbols: [AnalysisSymbol]
    let calls: [AnalysisCall]
    let hasSyntaxErrors: Bool
}

struct DocumentAnalysis: Decodable, Sendable {
    let path: String
    let revision: UInt64
    let language: String
    let level: String
    let facts: AnalysisFacts
}

struct AnalysisUpdate: Decodable, Sendable {
    let revision: UInt64
    let mode: String
    let parseMicros: UInt64
    let extractMicros: UInt64
    let totalMicros: UInt64
    let changedRangeCount: Int
}

struct OpenDocument: Decodable, Sendable {
    let document: SourceDocument
    let analysis: DocumentAnalysis?
    let update: AnalysisUpdate?
    let analysisError: CoreFault?
    let storage: StorageInfo?
}

struct StorageInfo: Decodable, Sendable { let source: String; let loadMicros: UInt64; let persistence: String }
struct CoreAck: Decodable, Sendable { let queued: Bool?; let accepted: Bool?; let flushed: Bool? }
struct CacheSettingsInfo: Decodable, Sendable { let mode: String; let maxBytes: Int; let prefetch: Bool }
struct CacheMetrics: Decodable, Sendable {
    let effectiveBytes: Int; let factLimitBytes: Int; let usedBytes: Int; let entries: Int
    let hits: UInt64; let misses: UInt64; let prefetchAdmitted: UInt64; let prefetchUseful: UInt64; let pressure: String
}
struct CacheStatus: Decodable, Sendable { let settings: CacheSettingsInfo; let cache: CacheMetrics; let sqliteVersion: String }

struct AnalysisResult: Decodable, Sendable {
    let analysis: DocumentAnalysis
    let update: AnalysisUpdate
}

struct AnalysisContext: Decodable, Sendable {
    let revision: UInt64
    let symbol: AnalysisSymbol?
    let call: AnalysisCall?
}

struct SourceNavigation {
    let token: Int
    let path: String
    let range: AnalysisRange
}

struct CoreFault: Decodable, Error, LocalizedError, Sendable {
    let code: String
    let message: String
    var errorDescription: String? {
        let prefix: String
        switch code {
        case "binary_file": prefix = "当前阅读器暂不支持二进制文件"
        case "file_too_large": prefix = "当前阅读骨架仅支持不超过 2 MiB 的文件"
        case "unsupported_encoding": prefix = "当前阅读器仅支持 UTF-8 文本"
        case "stale_document": prefix = "文件内容已变化，请重新打开文件；未保存内容需使用语义预览"
        case "semantic_not_configured": prefix = "请先导入 C/C++ 编译配置"
        case "outside_workspace", "invalid_path": prefix = "文件不在当前工作空间范围内"
        default: prefix = "核心服务返回错误（\(code)）"
        }
        return "\(prefix)。\n\(message)"
    }
}

struct FileNode: Identifiable {
    let entry: FileEntry
    let children: [FileNode]?
    var id: String { entry.path }

    static func build(from entries: [FileEntry]) -> [FileNode] {
        let groups = Dictionary(grouping: entries) { entry in
            entry.path.split(separator: "/").dropLast().joined(separator: "/")
        }
        func nodes(in parent: String) -> [FileNode] {
            (groups[parent] ?? []).sorted { left, right in
                if left.isDirectory != right.isDirectory { return left.isDirectory }
                return left.name.localizedStandardCompare(right.name) == .orderedAscending
            }.map { entry in
                FileNode(entry: entry, children: entry.isDirectory ? nodes(in: entry.path) : nil)
            }
        }
        return nodes(in: "")
    }
}

enum LaunchOptions {
    static func value(after flag: String) -> String? {
        let arguments = CommandLine.arguments
        guard let index = arguments.firstIndex(of: flag), arguments.indices.contains(index + 1) else { return nil }
        return arguments[index + 1]
    }
    static var isCapturing: Bool { value(after: "--capture-to") != nil }
    static var isProfiling: Bool { value(after: "--perf-idle-to") != nil }
    static var isDiagnostic: Bool { isCapturing || isProfiling }
    static func profileEvent(_ phase: String) {
        guard isProfiling else { return }
        FileHandle.standardError.write(Data("PERF_PROBE \(phase)\n".utf8))
    }
}
