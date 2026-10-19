import Foundation

struct SemanticLocation: Codable, Sendable {
    let path: String; let offset: Int; let endOffset: Int; let line: Int; let column: Int
}
struct SemanticSymbol: Codable, Sendable, Identifiable {
    let id: String; let usr: String; let name: String; let kind: String; let signature: String
    let definition: Bool; let callable: Bool; let location: SemanticLocation; let extent: SemanticLocation
    let unitId: String?; let mainFile: String?
}
struct SemanticReference: Codable, Sendable {
    let target: String; let name: String; let location: SemanticLocation; let unitId: String?
}
struct SemanticCall: Codable, Sendable {
    let caller: String?; let callerName: String?; let target: String?; let name: String
    let resolution: String; let location: SemanticLocation; let unitId: String?; let mainFile: String?
    var resolutionLabel: String {
        switch resolution { case "direct": "直接调用"; case "virtual_candidate": "虚调用候选"; case "indirect": "函数指针 · 目标未知"; default: "未解析" }
    }
}
struct SemanticDocumentRevision: Codable, Sendable, Hashable {
    let path: String
    let revision: UInt64
}
struct SemanticQueryCoverage: Codable, Sendable, Hashable {
    let type: String?
    let unitId: String?
}
struct SemanticQueryVersion: Codable, Sendable, Hashable {
    let schema: String
    let workspace: String
    let configuration: String
    let provider: String
    let diskBaseline: [String: UInt64]?
    let documents: [SemanticDocumentRevision]
    let coverage: SemanticQueryCoverage?
}
struct SemanticIncluderCoverage: Codable, Sendable, Hashable {
    let scope: String
    let knownIncluders: [String]
    let updatedIncluders: [String]
    let staleIncluders: [String]
}
struct SemanticDiagnostic: Codable, Sendable {
    let severity: Int?
    let message: String
    let location: SemanticLocation?
}
struct SemanticFailedUnit: Codable, Sendable {
    let unitId: String
    let mainFile: String
    let reason: String
    let kind: String
    let diagnostics: [SemanticDiagnostic]
}
struct SemanticPreviewDocument: Codable, Sendable {
    let path: String
    let revision: UInt64
}
struct SemanticContext: Codable, Sendable {
    let entityId: String?; let declarations: [SemanticSymbol]; let references: [SemanticReference]
    let enclosingFunctions: [SemanticSymbol]
    let overlayUnitId: String?
    let completeness: String?
    let queryVersion: SemanticQueryVersion?
    let includerCoverage: SemanticIncluderCoverage?
    let failedUnits: [SemanticFailedUnit]?
}
struct SemanticRows<T: Codable & Sendable>: Codable, Sendable {
    let rows: [T]
    let entityId: String?
    let direction: String?
    let completeness: String?
    let queryVersion: SemanticQueryVersion?
    let includerCoverage: SemanticIncluderCoverage?
    let failedUnits: [SemanticFailedUnit]?
    let limit: Int?
    let offset: Int?
    let total: Int?
    let hasMore: Bool?
    let truncated: Bool?
    let truncationReason: String?
}
struct SemanticFailure: Codable, Sendable { let file: String?; let error: String? }
struct SemanticJob: Codable, Sendable {
    let state: String; let total: Int?; let completed: Int?; let analyzed: Int?; let reused: Int?
    let failures: [SemanticFailure]?
    let trigger: String?
    let wallMs: UInt64?
}
struct SemanticWatch: Codable, Sendable {
    let enabled: Bool; let autoRefresh: Bool; let metadataCache: Bool
    let pendingUnits: Int; let paused: Bool; let pressurePaused: Bool; let revision: Int
    let manualRefreshRequired: Bool; let configurationRequired: Bool; let error: String?
    let fingerprints: SemanticFingerprints?; let configurationFingerprints: SemanticFingerprints?
    let autoJobs: UInt64?
}
struct SemanticFingerprints: Codable, Sendable {
    let hashes: UInt64; let bytesRead: UInt64
}
struct SemanticProgress: Codable, Sendable {
    let configured: Bool; let configuredUnits: Int?; let job: SemanticJob?
    let incompatibleProviderUnits: Int?
    let watch: SemanticWatch?
    let preview: SemanticPreview?
}
struct SemanticPreviewTicket: Codable, Sendable {
    let jobId: UInt64; let path: String; let revision: UInt64; let unitId: String
}
struct SemanticPreview: Codable, Sendable {
    let state: String; let ticket: SemanticPreviewTicket?; let reason: String?
    let workerActive: Bool; let queued: Bool; let queueLen: Int?; let queuedUnits: [String]?
    let chargedBytes: UInt64; let memoryBudgetBytes: UInt64
    let wallMs: UInt64?
    let completeness: String?
    let includerCoverage: SemanticIncluderCoverage?
    let failedUnits: [SemanticFailedUnit]?
    let queryVersion: SemanticQueryVersion?
    let projectQueryVersion: SemanticQueryVersion?
}
