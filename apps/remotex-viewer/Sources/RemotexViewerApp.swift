import Foundation
import SwiftUI

@main
struct RemotexViewerApp: App {
    @State private var model = AppModel()

    init() {
        if CommandLine.arguments.contains("--version") {
            print("remotex-viewer \(ProductInfo.version)")
            Foundation.exit(EXIT_SUCCESS)
        }
    }

    var body: some Scene {
        WindowGroup {
            ContentView(model: model)
                .frame(minWidth: 900, minHeight: 640)
        }
        .defaultSize(width: 1440, height: 900)
        .commands {
            RemoteCommands(model: model)
        }

        Settings {
            SettingsView(model: model)
        }
    }
}
