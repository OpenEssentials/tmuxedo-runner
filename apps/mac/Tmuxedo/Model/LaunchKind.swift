import CoreServices
import Foundation

/// Why the app was launched, which decides whether to open the dashboard.
///
/// A person opening the app from Finder, Launchpad, Spotlight or the Dock
/// wants to SEE it, and on a full menu bar macOS hides the status item, so
/// without a window the app would appear to do nothing at all. A login-item
/// launch must stay silent: a window on every login is wrong for an app whose
/// job is to keep this Mac connected in the background.
enum LaunchKind {
    /// Posted by anything that wants the dashboard (the popover's button); the app delegate owns the window.
    static let showDashboard = Notification.Name("Tmuxedo.showDashboard")
    /// Posted by anything that wants the "Pair this Mac" sheet.
    static let showPairing = Notification.Name("Tmuxedo.showPairing")

    /// True when macOS started the app as a login item.
    ///
    /// The launch Apple event (`oapp`) carries `keyAEPropData =
    /// keyAELaunchedAsLogInItem` in that case. It must be read in
    /// `applicationWillFinishLaunching`: by `didFinishLaunching` the current
    /// event may already be gone.
    static func isLoginItem(_ event: NSAppleEventDescriptor?) -> Bool {
        guard let event,
              event.eventClass == AEEventClass(kCoreEventClass),
              event.eventID == AEEventID(kAEOpenApplication),
              let prop = event.paramDescriptor(forKeyword: AEKeyword(keyAEPropData))
        else { return false }
        return prop.enumCodeValue == OSType(keyAELaunchedAsLogInItem)
    }
}
