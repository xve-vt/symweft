import AppKit
import SwiftUI

@main
struct SymweftApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var appearance = AppearanceController()
    @StateObject private var store = WorkspaceStore()

    var body: some Scene {
        Window("Symweft", id: "workbench") {
            WorkbenchView()
                .environmentObject(store)
                .environmentObject(appearance)
                .frame(minWidth: 1000, minHeight: 640)
                .task {
                    LaunchOptions.profileEvent("bootstrap_start")
                    await store.bootstrap()
                    LaunchOptions.profileEvent("bootstrap_complete")
                    await store.profileIdleIfRequested()
                    await store.captureIfRequested()
                }
        }
        .defaultSize(width: 1280, height: 800)
        .windowToolbarStyle(.unifiedCompact)
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("打开工作空间…") { store.chooseWorkspace() }
                    .keyboardShortcut("o", modifiers: [.command, .shift])
            }
            CommandMenu("工作空间") {
                Button("导入 C/C++ 编译配置…") { store.chooseCompileCommands() }
                    .disabled(store.workspace == nil)
                Button("刷新文件树") { store.refresh() }
                    .keyboardShortcut("r", modifiers: [.command, .shift])
                    .disabled(store.workspace == nil || store.isOpening)
                Toggle("显示文件信息", isOn: $store.showInspector)
                    .keyboardShortcut("i", modifiers: [.command, .option])
            }
        }
        Settings {
            SettingsView().environmentObject(appearance).environmentObject(store)
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        LaunchOptions.profileEvent("app_launched")
        if !LaunchOptions.isCapturing { NSApp.activate(ignoringOtherApps: true) }
    }
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}
