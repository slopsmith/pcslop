// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "pcslop-manager",
    platforms: [.macOS(.v26)],
    targets: [
        .target(name: "ManagerCore"),
        .testTarget(name: "ManagerCoreTests", dependencies: ["ManagerCore"]),
    ]
)
