import SwiftUI

@main
struct TmuxedoApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) var appDelegate

    var body: some Scene {
        // The menu bar IS the app (LSUIElement). Settings is the only SwiftUI
        // scene; the dashboard and the pairing sheet are windows the delegate
        // owns. Qualified because the app has its own `Settings` model.
        SwiftUI.Settings {
            SettingsView()
                .environmentObject(appDelegate.model)
                .environmentObject(appDelegate.settings)
                .environmentObject(appDelegate.model.supervisor)
                .environmentObject(appDelegate.updates)
        }
    }
}
