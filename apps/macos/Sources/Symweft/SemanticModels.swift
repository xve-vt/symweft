import Foundation

struct SemanticLocation: Decodable, Sendable {
    let path: String; let offset: Int; let endOffset: Int; let line: Int; let column: Int
}
struct SemanticSymbol: Decodable, Sendable, Identifiable {
    let id: String; let usr: String; let name: String; let kind: String; let signature: String
    let definition: Bool; let callable: Bool; let location: SemanticLocation; let extent: SemanticLocation
    let unitId: String?; let mainFile: String?
}
struct SemanticReference: Decodable, Sendable {
    let target: String; let name: String; let location: SemanticLocation; let unitId: String?
}
struct SemanticCall: Decodable, Sendable {
    let caller: String?; let callerName: String?; let target: String?; let name: String
    let resolution: String; let location: SemanticLocation; let unitId: String?; let mainFile: String?
    var resolutionLabel: String {
        switch resolution { case "direct": "直接调用"; case "virtual_candidate": "虚调用候选"; case "indirect": "函数指针 · 目标未知"; default: "未解析" }
    }
}
struct SemanticContext: Decodable, Sendable {
    let entityId: String?; let declarations: [SemanticSymbol]; let references: [SemanticReference]
    let enclosingFunctions: [SemanticSymbol]
}
struct SemanticRows<T: Decodable & Sendable>: Decodable, Sendable { let rows: [T] }
struct SemanticFailure: Decodable, Sendable { let file: String?; let error: String? }
struct SemanticJob: Decodable, Sendable {
    let state: String; let total: Int?; let completed: Int?; let analyzed: Int?; let reused: Int?
    let failures: [SemanticFailure]?
    let trigger: String?
    let wallMs: UInt64?
}
struct SemanticWatch: Decodable, Sendable {
    let enabled: Bool; let autoRefresh: Bool; let metadataCache: Bool
    let pendingUnits: Int; let paused: Bool; let pressurePaused: Bool; let revision: Int
    let manualRefreshRequired: Bool; let configurationRequired: Bool; let error: String?
    let fingerprints: SemanticFingerprints?; let configurationFingerprints: SemanticFingerprints?
    let autoJobs: UInt64?
}
struct SemanticFingerprints: Decodable, Sendable {
    let hashes: UInt64; let bytesRead: UInt64
}
struct SemanticProgress: Decodable, Sendable {
    let configured: Bool; let configuredUnits: Int?; let job: SemanticJob?
    let incompatibleProviderUnits: Int?
    let watch: SemanticWatch?
    let preview: SemanticPreview?
}
struct SemanticPreviewTicket: Decodable, Sendable {
    let jobId: UInt64; let path: String; let revision: UInt64; let unitId: String
}
struct SemanticPreview: Decodable, Sendable {
    let state: String; let ticket: SemanticPreviewTicket?; let reason: String?
    let workerActive: Bool; let queued: Bool; let chargedBytes: UInt64; let memoryBudgetBytes: UInt64
    let wallMs: UInt64?
}
