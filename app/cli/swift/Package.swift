// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "pocketskynet-swift",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "PocketSkynetClient", targets: ["PocketSkynetClient"]),
        .executable(name: "pskynet-swift", targets: ["pskynet-swift"]),
    ],
    dependencies: [
        .package(url: "https://github.com/apple/swift-argument-parser.git", from: "1.3.0"),
        .package(url: "https://github.com/GigaBitcoin/secp256k1.swift.git", .upToNextMinor(from: "0.18.0")),
    ],
    targets: [
        .target(
            name: "PocketSkynetClient",
            dependencies: [
                .product(name: "secp256k1", package: "secp256k1.swift")
            ]
        ),
        .executableTarget(
            name: "pskynet-swift",
            dependencies: [
                "PocketSkynetClient",
                .product(name: "ArgumentParser", package: "swift-argument-parser"),
            ]
        ),
        .testTarget(
            name: "PocketSkynetClientTests",
            dependencies: ["PocketSkynetClient"]
        ),
        .testTarget(
            name: "IntegrationTests",
            dependencies: ["PocketSkynetClient"]
        ),
    ]
)
