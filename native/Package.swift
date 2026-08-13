// swift-tools-version: 6.1
import PackageDescription

let package = Package(
    name: "LishNative",
    platforms: [
        .macOS(.v14),
        .iOS(.v17),
    ],
    products: [
        .library(name: "LishDisk", targets: ["LishDisk"]),
        .library(name: "LishNetwork", targets: ["LishNetwork"]),
        .executable(name: "lish-network-host", targets: ["LishNetworkHost"]),
    ],
    targets: [
        .target(
            name: "LishDisk",
            linkerSettings: [.linkedFramework("Network")]
        ),
        .systemLibrary(
            name: "CLibSlirp",
            pkgConfig: "slirp",
            providers: [.brew(["libslirp"])]
        ),
        .target(
            name: "CLishSlirp",
            dependencies: ["CLibSlirp"],
            publicHeadersPath: "include"
        ),
        .target(
            name: "LishNetwork",
            dependencies: ["CLishSlirp"],
            linkerSettings: [.linkedFramework("Network")]
        ),
        .executableTarget(
            name: "LishNetworkHost",
            dependencies: ["LishNetwork"]
        ),
        .testTarget(
            name: "LishDiskTests",
            dependencies: ["LishDisk"]
        ),
        .testTarget(
            name: "LishNetworkTests",
            dependencies: ["LishNetwork"]
        ),
    ]
)
