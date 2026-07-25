import Foundation

enum ProductInfo {
    static let bridgeVersion = 4
    static let developmentVersion = "0.0.27"

    static var version: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
            ?? developmentVersion
    }
}
