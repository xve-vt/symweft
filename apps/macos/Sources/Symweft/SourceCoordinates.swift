import Foundation

/// Line anchors avoid transcoding the entire prefix on each caret movement.
struct SourceCoordinates {
    private let source: String
    private let byteStarts: [Int]
    private let utf16Starts: [Int]
    private let utf16Length: Int

    init(_ source: String) {
        self.source = source
        var bytes = [0], units = [0]
        var byte = 0, unit = 0
        for scalar in source.unicodeScalars {
            byte += scalar.utf8.count
            unit += scalar.value > 0xFFFF ? 2 : 1
            if scalar.value == 10 { bytes.append(byte); units.append(unit) }
        }
        byteStarts = bytes
        utf16Starts = units
        utf16Length = unit
    }

    private func line(at offset: Int, starts: [Int]) -> Int {
        var lower = 0, upper = starts.count
        while lower < upper {
            let middle = (lower + upper) / 2
            if starts[middle] <= offset { lower = middle + 1 } else { upper = middle }
        }
        return max(0, lower - 1)
    }

    func byteOffset(utf16 offset: Int) -> Int? {
        guard offset >= 0, offset <= utf16Length else { return nil }
        let row = line(at: offset, starts: utf16Starts)
        let byteAnchor = source.utf8.index(source.utf8.startIndex, offsetBy: byteStarts[row])
        guard let anchor = byteAnchor.samePosition(in: source.utf16) else { return nil }
        let index = source.utf16.index(anchor, offsetBy: offset - utf16Starts[row])
        guard let byteIndex = index.samePosition(in: source.utf8) else { return nil }
        return byteStarts[row] + source.utf8.distance(from: byteAnchor, to: byteIndex)
    }

    func utf16Offset(byte offset: Int) -> Int? {
        guard offset >= 0, offset <= source.utf8.count else { return nil }
        let row = line(at: offset, starts: byteStarts)
        let byteAnchor = source.utf8.index(source.utf8.startIndex, offsetBy: byteStarts[row])
        let byteIndex = source.utf8.index(byteAnchor, offsetBy: offset - byteStarts[row])
        guard let anchor = byteAnchor.samePosition(in: source.utf16), let index = byteIndex.samePosition(in: source.utf16) else { return nil }
        return utf16Starts[row] + source.utf16.distance(from: anchor, to: index)
    }

    func nsRange(_ range: AnalysisRange) -> NSRange? {
        guard let start = utf16Offset(byte: range.startByte), let end = utf16Offset(byte: range.endByte), end >= start else { return nil }
        return NSRange(location: start, length: end - start)
    }
    func anchor(byte offset: Int) -> (line: Int, column: Int) {
        let safe = min(max(0, offset), source.utf8.count)
        let row = line(at: safe, starts: byteStarts)
        return (row, safe - byteStarts[row])
    }
    func byteOffset(line row: Int, column: Int) -> Int {
        let row = min(max(0, row), byteStarts.count - 1)
        let end = row + 1 < byteStarts.count ? byteStarts[row + 1] - 1 : source.utf8.count
        var offset = min(end, byteStarts[row] + max(0, column))
        while offset > byteStarts[row], utf16Offset(byte: offset) == nil { offset -= 1 }
        return offset
    }
}
