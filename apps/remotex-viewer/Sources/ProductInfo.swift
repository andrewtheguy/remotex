import Foundation

enum ProductInfo {
    static let bridgeVersion = 5
    static let developmentVersion = "0.0.29"

    static var version: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
            ?? developmentVersion
    }
}
