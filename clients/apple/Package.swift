// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "ParkCore",
    platforms: [.iOS(.v15), .macOS(.v12)],
    products: [.library(name: "ParkCore", targets: ["ParkCore"])],
    targets: [
        .binaryTarget(name: "park_ffiFFI", path: "Generated/park_ffiFFI.xcframework"),
        .target(name: "ParkCore", dependencies: ["park_ffiFFI"], path: "Sources/ParkCore",
                linkerSettings: [.linkedFramework("Security"), .linkedFramework("CoreFoundation")])
    ]
)
