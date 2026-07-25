import Foundation

enum ProductInfo {
    static let bridgeVersion = 3
    static let developmentVersion = "0.0.26"

    static var version: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
            ?? developmentVersion
    }
}
