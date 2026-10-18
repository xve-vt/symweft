import AppKit
import SwiftUI

struct SettingsView: View {
    @EnvironmentObject private var appearance: AppearanceController
    @EnvironmentObject private var store: WorkspaceStore
    var body: some View {
        Form {
            Picker("外观", selection: $appearance.choice) {
                ForEach(AppearanceChoice.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.radioGroup)
            Text("默认随 macOS 实时切换。源码区域采用较轻的中性底色，避免原设计稿过深的蓝黑背景。")
                .font(.callout)
                .foregroundStyle(.secondary)
            Section("索引热缓存") {
                Picker("预算模式", selection: $store.cacheMode) {
                    Text("自动调整").tag("auto")
                    Text("手动上限").tag("manual")
                    Text("关闭附加缓存").tag("off")
                }
                Stepper("上限：\(store.cacheMiB) MiB", value: $store.cacheMiB, in: 8...1024, step: 8)
                    .disabled(store.cacheMode == "off")
                Toggle("预读已索引的本地依赖", isOn: $store.prefetchEnabled)
                Toggle("记录本地阅读关注度", isOn: $store.collectAttention)
                Button("清空当前项目的阅读关注记录") { store.clearAttention() }
                    .disabled(store.workspace == nil)
                if let status = store.cacheStatus {
                    LabeledContent("当前预算", value: "\(status.cache.effectiveBytes / 1024 / 1024) MiB")
                    LabeledContent("已解码事实", value: ByteCountFormatter.string(fromByteCount: Int64(status.cache.usedBytes), countStyle: .memory))
                    LabeledContent("压力状态", value: status.cache.pressure)
                }
                Text("自动模式在上限内调整，内存压力时缩减并暂停预读。此预算不代表进程 RSS 上限；活动源码、语法树与 UI 另计。阅读关注只统计有效前台停留，不上传。")
                    .font(.caption).foregroundStyle(.secondary)
            }
            LabeledContent("当前版本", value: "0.1.0 · 语法分析与持久化")
        }
        .formStyle(.grouped)
        .padding(12)
        .frame(width: 500, height: 570)
    }
}
