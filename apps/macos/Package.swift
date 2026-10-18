// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "Symweft",
    platforms: [.macOS(.v14)],
    products: [.executable(name: "Symweft", targets: ["Symweft"])],
    targets: [
        .executableTarget(name: "Symweft"),
    ]
)
