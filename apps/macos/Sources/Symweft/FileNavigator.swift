import AppKit
import SwiftUI

/// Native tree navigation, including keyboard focus, accessibility and disclosure.
struct FileNavigator: NSViewRepresentable {
    let tree: [FileNode]
    let generation: Int
    let selection: String?
    let onSelect: (String?) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(onSelect: onSelect) }

    func makeNSView(context: Context) -> NSScrollView {
        let scroll = NSScrollView()
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.drawsBackground = true
        scroll.backgroundColor = .windowBackgroundColor
        let outline = NSOutlineView()
        outline.frame = NSRect(x: 0, y: 0, width: 260, height: 400)
        outline.autoresizingMask = [.width]
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("file"))
        column.resizingMask = .autoresizingMask
        outline.addTableColumn(column)
        outline.outlineTableColumn = column
        outline.headerView = nil
        outline.style = .sourceList
        outline.rowHeight = 27
        outline.indentationPerLevel = 14
        outline.intercellSpacing = .zero
        outline.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        outline.allowsEmptySelection = true
        outline.backgroundColor = .windowBackgroundColor
        outline.setAccessibilityLabel("工作空间文件树")
        outline.dataSource = context.coordinator
        outline.delegate = context.coordinator
        scroll.documentView = outline
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        guard let outline = scroll.documentView as? NSOutlineView else { return }
        let coordinator = context.coordinator
        coordinator.onSelect = onSelect
        if coordinator.generation != generation {
            var expanded = Set<String>()
            for row in 0..<outline.numberOfRows {
                if let node = outline.item(atRow: row) as? Node, outline.isItemExpanded(node) { expanded.insert(node.id) }
            }
            coordinator.generation = generation
            coordinator.nodes = tree.map(Node.init)
            coordinator.mutingSelection = true
            outline.reloadData()
            @MainActor func restore(_ nodes: [Node]) {
                for node in nodes {
                    let shouldExpand: Bool
                    if expanded.isEmpty { shouldExpand = !node.children.isEmpty }
                    else { shouldExpand = expanded.contains(node.id) }
                    if shouldExpand {
                        outline.expandItem(node)
                        if !expanded.isEmpty { restore(node.children) }
                    }
                }
            }
            restore(coordinator.nodes)
            coordinator.mutingSelection = false
        }
        let row = (0..<outline.numberOfRows).first { (outline.item(atRow: $0) as? Node)?.id == selection }
        coordinator.mutingSelection = true
        if let row { outline.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false) }
        else { outline.deselectAll(nil) }
        coordinator.mutingSelection = false
    }

    final class Node: NSObject {
        let entry: FileEntry
        let children: [Node]
        var id: String { entry.path }
        init(_ node: FileNode) {
            entry = node.entry
            children = node.children?.map(Node.init) ?? []
        }
    }

    final class Coordinator: NSObject, NSOutlineViewDataSource, NSOutlineViewDelegate {
        var nodes: [Node] = []
        var generation = -1
        var mutingSelection = false
        var onSelect: (String?) -> Void
        init(onSelect: @escaping (String?) -> Void) { self.onSelect = onSelect }

        func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
            (item as? Node)?.children.count ?? nodes.count
        }
        func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
            ((item as? Node)?.children ?? nodes)[index]
        }
        func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool {
            (item as? Node)?.entry.isDirectory ?? false
        }
        func outlineView(_ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any) -> NSView? {
            guard let node = item as? Node else { return nil }
            let identifier = NSUserInterfaceItemIdentifier("fileCell")
            let cell = (outlineView.makeView(withIdentifier: identifier, owner: self) as? NSTableCellView) ?? makeCell(identifier)
            cell.textField?.stringValue = node.entry.name
            cell.toolTip = node.entry.path
            cell.imageView?.image = NSImage(systemSymbolName: node.entry.isDirectory ? "folder" : "doc.text", accessibilityDescription: node.entry.isDirectory ? "目录" : "文件")
            cell.imageView?.contentTintColor = node.entry.isDirectory ? .controlAccentColor : .secondaryLabelColor
            return cell
        }
        @MainActor private func makeCell(_ identifier: NSUserInterfaceItemIdentifier) -> NSTableCellView {
            let cell = NSTableCellView()
            cell.identifier = identifier
            let icon = NSImageView()
            let label = NSTextField(labelWithString: "")
            icon.translatesAutoresizingMaskIntoConstraints = false
            label.translatesAutoresizingMaskIntoConstraints = false
            label.font = .systemFont(ofSize: 13)
            label.lineBreakMode = .byTruncatingMiddle
            cell.addSubview(icon)
            cell.addSubview(label)
            cell.imageView = icon
            cell.textField = label
            NSLayoutConstraint.activate([
                icon.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 2),
                icon.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
                icon.widthAnchor.constraint(equalToConstant: 16),
                icon.heightAnchor.constraint(equalToConstant: 16),
                label.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 7),
                label.trailingAnchor.constraint(equalTo: cell.trailingAnchor, constant: -5),
                label.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
            ])
            return cell
        }
        func outlineViewSelectionDidChange(_ notification: Notification) {
            guard !mutingSelection, let outline = notification.object as? NSOutlineView else { return }
            onSelect(outline.selectedRow >= 0 ? (outline.item(atRow: outline.selectedRow) as? Node)?.id : nil)
        }
    }
}
