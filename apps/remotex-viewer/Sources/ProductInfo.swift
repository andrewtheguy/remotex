import Foundation

enum ProductInfo {
    static let bridgeVersion = 6
    static let developmentVersion = "0.0.30"

    static var version: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
            ?? developmentVersion
    }
}
