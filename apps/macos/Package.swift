// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "pcslop-manager",
    platforms: [.macOS(.v26)],
    products: [
        .executable(name: "pcslop-manager", targets: ["Manager"])
    ],
    targets: [
        .target(name: "ManagerCore"),
        .executableTarget(name: "Manager", dependencies: ["ManagerCore"], resources: [.copy("Logos")]),
        .testTarget(name: "ManagerCoreTests", dependencies: ["ManagerCore"]),
    ]
)
