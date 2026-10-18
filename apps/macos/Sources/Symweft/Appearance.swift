import AppKit
import SwiftUI

enum AppearanceChoice: String, CaseIterable, Identifiable {
    case system, light, dark
    var id: String { rawValue }
    var title: String {
        switch self {
        case .system: "跟随系统"
        case .light: "浅色"
        case .dark: "深色"
        }
    }
    var nsAppearance: NSAppearance? {
        switch self {
        case .system: nil
        case .light: NSAppearance(named: .aqua)
        case .dark: NSAppearance(named: .darkAqua)
        }
    }
}

@MainActor
final class AppearanceController: ObservableObject {
    @Published var choice: AppearanceChoice {
        didSet {
            if !LaunchOptions.isDiagnostic { UserDefaults.standard.set(choice.rawValue, forKey: "appearance") }
            apply()
        }
    }
    init() {
        let value = LaunchOptions.value(after: "--appearance") ?? UserDefaults.standard.string(forKey: "appearance") ?? "system"
        choice = AppearanceChoice(rawValue: value) ?? .system
        apply()
    }
    private func apply() {
        // nil clears an override; AppKit then tracks system appearance live.
        NSApplication.shared.appearance = choice.nsAppearance
    }
}

enum SourcePalette {
    static var background: NSColor {
        NSColor(name: nil) { appearance in
            if appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua {
                NSColor(srgbRed: 0.145, green: 0.153, blue: 0.169, alpha: 1)
            } else {
                NSColor(srgbRed: 0.980, green: 0.980, blue: 0.984, alpha: 1)
            }
        }
    }
}

