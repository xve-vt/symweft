import AppKit
import SwiftUI

struct SourceReader: NSViewRepresentable {
    let document: SourceDocument
    let navigation: SourceNavigation?
    let onCaret: (String, Int) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(onCaret: onCaret) }

    func makeNSView(context: Context) -> NSScrollView {
        let scroll = SourceScrollView()
        scroll.hasVerticalScroller = true
        scroll.hasHorizontalScroller = true
        scroll.autohidesScrollers = true
        scroll.borderType = .noBorder
        let storage = NSTextStorage()
        let layout = NSLayoutManager()
        storage.addLayoutManager(layout)
        let container = NSTextContainer(size: NSSize(width: 10_000_000, height: 10_000_000))
        container.widthTracksTextView = false
        layout.addTextContainer(container)
        let text = SourceTextView(frame: .zero, textContainer: container)
        text.delegate = context.coordinator
        text.isEditable = false
        text.isSelectable = true
        text.isRichText = false
        text.isAutomaticQuoteSubstitutionEnabled = false
        text.isAutomaticDashSubstitutionEnabled = false
        text.isGrammarCheckingEnabled = false
        text.isContinuousSpellCheckingEnabled = false
        text.usesFindBar = true
        text.isVerticallyResizable = true
        text.isHorizontallyResizable = true
        text.minSize = .zero
        text.maxSize = NSSize(width: 10_000_000, height: 10_000_000)
        text.autoresizingMask = [.width]
        text.textContainerInset = NSSize(width: 12, height: 12)
        text.font = NSFont.monospacedSystemFont(ofSize: 13, weight: .regular)
        text.setAccessibilityLabel("源码阅读区")
        text.applyPalette()
        scroll.documentView = text
        let ruler = LineNumberRuler(scrollView: scroll, textView: text)
        scroll.verticalRulerView = ruler
        scroll.hasVerticalRuler = true
        scroll.rulersVisible = true
        scroll.contentView.postsBoundsChangedNotifications = true
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        context.coordinator.onCaret = onCaret
        guard let text = scroll.documentView as? SourceTextView else { return }
        if text.filePath != document.path || text.string != document.content {
            text.filePath = document.path
            text.coordinates = SourceCoordinates(document.content)
            text.string = document.content
            text.applyPalette()
            scroll.contentView.scroll(to: .zero)
            scroll.reflectScrolledClipView(scroll.contentView)
            (scroll.verticalRulerView as? LineNumberRuler)?.updateLines(document.content)
        }
        if let navigation, navigation.path == document.path,
           text.navigationToken != navigation.token,
           let range = text.coordinates?.nsRange(navigation.range) {
            text.navigationToken = navigation.token
            text.window?.makeFirstResponder(text)
            text.setSelectedRange(range)
            let anchor = NSRange(location: range.location, length: min(1, range.length))
            text.scrollRangeToVisible(anchor)
            let token = navigation.token
            // Selection scrolling is deferred by AppKit. Once it settles, keep
            // line prefixes visible when the destination fits without panning.
            DispatchQueue.main.async { [weak text, weak scroll] in
                guard let text, let scroll, text.navigationToken == token,
                      let layout = text.layoutManager, let container = text.textContainer else { return }
                let glyphs = layout.glyphRange(forCharacterRange: anchor, actualCharacterRange: nil)
                let target = layout.boundingRect(forGlyphRange: glyphs, in: container)
                if target.maxX + text.textContainerInset.width < scroll.contentView.bounds.width {
                    let clip = scroll.contentView
                    clip.scroll(to: NSPoint(x: 0, y: clip.bounds.origin.y))
                    scroll.reflectScrolledClipView(clip)
                }
            }
        }
    }

    @MainActor
    final class Coordinator: NSObject, NSTextViewDelegate {
        var onCaret: (String, Int) -> Void
        init(onCaret: @escaping (String, Int) -> Void) { self.onCaret = onCaret }
        func textViewDidChangeSelection(_ notification: Notification) {
            guard let text = notification.object as? SourceTextView, let path = text.filePath,
                  let byte = text.coordinates?.byteOffset(utf16: text.selectedRange().location) else { return }
            let callback = onCaret
            Task { @MainActor in callback(path, byte) }
        }
    }
}

final class SourceScrollView: NSScrollView {
    private var isTilingSource = false
    override func tile() {
        guard !isTilingSource else { return }
        isTilingSource = true
        defer { isTilingSource = false }
        super.tile()
        guard rulersVisible, let ruler = verticalRulerView, let text = documentView as? NSTextView else { return }
        // Reserve actual clip space. An inset alone is invisible to AppKit's
        // delayed selection scrolling and lets the ruler cover the line prefix.
        let overlap = max(0, ruler.frame.maxX - contentView.frame.minX)
        if overlap > 0 {
            var frame = contentView.frame
            frame.origin.x += overlap
            frame.size.width = max(0, frame.size.width - overlap)
            contentView.frame = frame
        }
        let inset = NSSize(width: 12, height: 12)
        if text.textContainerInset != inset { text.textContainerInset = inset }
    }
}

final class SourceTextView: NSTextView {
    var filePath: String?
    var coordinates: SourceCoordinates?
    var navigationToken: Int?
    func applyPalette() {
        backgroundColor = SourcePalette.background
        textColor = .textColor
        insertionPointColor = .textColor
        selectedTextAttributes = [
            .backgroundColor: NSColor.selectedTextBackgroundColor,
            .foregroundColor: NSColor.selectedTextColor,
        ]
        enclosingScrollView?.backgroundColor = SourcePalette.background
        needsDisplay = true
    }
    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        applyPalette()
        enclosingScrollView?.verticalRulerView?.needsDisplay = true
    }
}

final class LineNumberRuler: NSRulerView {
    private weak var text: NSTextView?
    private var lineStarts = [0]
    override var isFlipped: Bool { true }

    init(scrollView: NSScrollView, textView: NSTextView) {
        text = textView
        super.init(scrollView: scrollView, orientation: .verticalRuler)
        clientView = textView
        ruleThickness = 52
        NotificationCenter.default.addObserver(
            self, selector: #selector(scrolled), name: NSView.boundsDidChangeNotification,
            object: scrollView.contentView
        )
    }
    required init(coder: NSCoder) { fatalError("Programmatic ruler only") }
    deinit { NotificationCenter.default.removeObserver(self) }
    @objc private func scrolled() { needsDisplay = true }

    func updateLines(_ content: String) {
        lineStarts = [0]
        for (index, unit) in content.utf16.enumerated() where unit == 10 { lineStarts.append(index + 1) }
        ruleThickness = max(52, CGFloat(String(lineStarts.count).count) * 8 + 24)
        needsDisplay = true
    }
    private func lineNumber(at character: Int) -> Int {
        var lower = 0
        var upper = lineStarts.count
        while lower < upper {
            let middle = (lower + upper) / 2
            if lineStarts[middle] <= character { lower = middle + 1 } else { upper = middle }
        }
        return max(1, lower)
    }
    override func drawHashMarksAndLabels(in rect: NSRect) {
        SourcePalette.background.setFill()
        bounds.fill()
        guard let text, let layout = text.layoutManager, let container = text.textContainer else { return }
        let visible = text.visibleRect
        let glyphs = layout.glyphRange(forBoundingRect: visible, in: container)
        let attributes: [NSAttributedString.Key: Any] = [
            .font: NSFont.monospacedDigitSystemFont(ofSize: 11, weight: .regular),
            .foregroundColor: NSColor.secondaryLabelColor,
        ]
        layout.enumerateLineFragments(forGlyphRange: glyphs) { [self] fragment, _, _, range, _ in
            let character = layout.characterIndexForGlyph(at: range.location)
            let label = String(lineNumber(at: character)) as NSString
            let size = label.size(withAttributes: attributes)
            label.draw(at: NSPoint(
                x: ruleThickness - size.width - 12,
                y: fragment.minY + text.textContainerInset.height - visible.minY + 2
            ), withAttributes: attributes)
        }
        NSColor.separatorColor.setFill()
        NSRect(x: bounds.maxX - 1, y: bounds.minY, width: 1, height: bounds.height).fill()
    }
}
