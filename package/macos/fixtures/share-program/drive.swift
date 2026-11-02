import ApplicationServices
import CoreGraphics
import Foundation

guard CommandLine.arguments.count == 3, let pid = Int32(CommandLine.arguments[1]) else {
    fatalError("usage: drive <fixture pid> <destination folder>")
}
let folderName = CommandLine.arguments[2]
guard AXIsProcessTrusted() else {
    print("Accessibility permission is required for the fixture driver")
    exit(1)
}
let app = AXUIElementCreateApplication(pid)

func value(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
    var output: CFTypeRef?
    return AXUIElementCopyAttributeValue(element, name as CFString, &output) == .success ? output : nil
}

func elements(_ root: AXUIElement, _ depth: Int = 0) -> [AXUIElement] {
    guard depth < 16 else { return [] }
    let children = value(root, kAXChildrenAttribute as String) as? [AXUIElement] ?? []
    return [root] + children.flatMap { elements($0, depth + 1) }
}

func firstElement(_ root: AXUIElement, matching predicate: (AXUIElement) -> Bool) -> AXUIElement? {
    var pending = [(root, 0)]
    while let (element, depth) = pending.popLast() {
        if predicate(element) { return element }
        guard depth < 16 else { continue }
        let children = value(element, kAXChildrenAttribute as String) as? [AXUIElement] ?? []
        pending.append(contentsOf: children.reversed().map { ($0, depth + 1) })
    }
    return nil
}

func wait<T>(_ name: String, timeout: TimeInterval = 30, _ operation: () -> T?) -> T {
    let deadline = Date().addingTimeInterval(timeout)
    repeat {
        if let result = operation() { return result }
        Thread.sleep(forTimeInterval: 0.1)
    } while Date() < deadline
    print("timed out waiting for \(name)")
    exit(1)
}

func picker() -> AXUIElement? {
    let windows = value(app, kAXWindowsAttribute as String) as? [AXUIElement] ?? []
    for window in windows {
        let title = value(window, kAXTitleAttribute as String) as? String
        if title == "Add a program to share to" || title == "Open the capture with" {
            return window
        }
    }
    for window in windows {
        if let sheet = firstElement(window, matching: { element in
            (value(element, kAXRoleAttribute as String) as? String) == "AXSheet" &&
            (value(element, kAXDefaultButtonAttribute as String) != nil ||
             firstElement(element, matching: { (value($0, kAXTitleAttribute as String) as? String) == "Open" }) != nil)
        }) {
            return sheet
        }
    }
    return nil
}

func button(_ title: String, attribute: String, in root: AXUIElement) -> AXUIElement? {
    if let item = value(root, attribute), CFGetTypeID(item) == AXUIElementGetTypeID() {
        return (item as! AXUIElement)
    }
    let matches: (AXUIElement) -> Bool = {
        (value($0, kAXRoleAttribute as String) as? String) == "AXButton" &&
        (value($0, kAXTitleAttribute as String) as? String) == title
    }
    let children = value(root, kAXChildrenAttribute as String) as? [AXUIElement] ?? []
    return children.first(where: matches) ?? firstElement(root, matching: matches)
}

func press(_ title: String, attribute: String, in root: AXUIElement) -> Bool {
    guard let item = button(title, attribute: attribute, in: root),
          (value(item, kAXEnabledAttribute as String) as? Bool) == true else { return false }
    return AXUIElementPerformAction(item, kAXPressAction as CFString) == .success
}

if folderName.hasPrefix("prefs:") {
    let root = String(folderName.dropFirst("prefs:".count))
    func preferences(whenReady marker: String) -> AXUIElement {
        _ = wait("Preferences fixture", timeout: 30) {
            let log = try? String(contentsOfFile: root + "/ui-log.txt", encoding: .utf8)
            return log?.contains(marker) == true ? true : nil
        }
        return wait("Preferences window") {
            let windows = value(app, kAXWindowsAttribute as String) as? [AXUIElement] ?? []
            return windows.first { (value($0, kAXTitleAttribute as String) as? String) == "Jiffshot Preferences" }
        }
    }
    func submitPreferences(whenReady marker: String) {
        let preferences = preferences(whenReady: marker)
        let checkbox = wait("first destination checkbox", timeout: 10) {
            firstElement(preferences) { element in
                guard (value(element, kAXRoleAttribute as String) as? String) == "AXCheckBox" else { return false }
                return firstElement(element) { node in
                    [kAXTitleAttribute as String, kAXDescriptionAttribute as String, kAXValueAttribute as String].contains {
                        (value(node, $0) as? String) == "First Fixture"
                    }
                } != nil
            }
        }
        guard (value(checkbox, kAXValueAttribute as String) as? NSNumber)?.boolValue == true,
              AXUIElementPerformAction(checkbox, kAXPressAction as CFString) == .success else {
            print("could not clear the first destination")
            exit(1)
        }
        _ = wait("cleared destination") {
            (value(checkbox, kAXValueAttribute as String) as? NSNumber)?.boolValue == false ? true : nil
        }
        _ = wait("Preferences OK") { press("OK", attribute: kAXDefaultButtonAttribute as String, in: preferences) ? true : nil }
    }
    submitPreferences(whenReady: "ready to submit stale Share preferences")
    submitPreferences(whenReady: "ready to submit current Share preferences")
    func setFTP(whenReady marker: String, from old: String, to new: String) {
        let preferences = preferences(whenReady: marker)
        let ftp = wait("FTP field") {
            firstElement(preferences) { element in
                let role = value(element, kAXRoleAttribute as String) as? String
                let text = value(element, kAXValueAttribute as String) as? String
                return (role == "AXTextField" || role == "AXTextArea") && text == old
            }
        }
        guard AXUIElementSetAttributeValue(ftp, kAXValueAttribute as CFString, new as CFString) == .success else {
            print("could not update the FTP value")
            exit(1)
        }
        _ = wait("updated FTP field", timeout: 8) {
            var observed: CFTypeRef?
            let status = AXUIElementCopyAttributeValue(ftp, kAXValueAttribute as CFString, &observed)
            if status == .noValue && new.isEmpty { return true }
            return status == .success && (observed as? String) == new ? true : nil
        }
        _ = wait("Preferences OK") { press("OK", attribute: kAXDefaultButtonAttribute as String, in: preferences) ? true : nil }
    }
    setFTP(whenReady: "ready to correct the legacy FTP value",
           from: "  ftp://fixture@example.test/captures  ", to: "ftp://fixture@example.test/captures")
    setFTP(whenReady: "ready to clear the FTP value", from: "ftp://fixture@example.test/captures", to: "")
    print("submitted stale and current Share preferences and updated FTP")
    exit(0)
}

func selectApplication(in panel: AXUIElement) {
    let selection = wait("disposable application bundle") { () -> (AXUIElement, AXUIElement)? in
        for list in elements(panel) where (value(list, kAXRoleAttribute as String) as? String) == "AXList" {
            let groups = value(list, kAXChildrenAttribute as String) as? [AXUIElement] ?? []
            if let group = groups.first(where: { group in
                elements(group).contains { node in
                    let name = value(node, kAXValueAttribute as String) as? String ?? ""
                    return name == "Fixture Share" || name == "Fixture Share.app"
                }
            }) {
                return (list, group)
            }
        }
        return nil
    }
    let selectionStatus = AXUIElementSetAttributeValue(
        selection.0, kAXSelectedChildrenAttribute as CFString, [selection.1] as CFArray
    )
    if selectionStatus != .success {
        let selectedStatus = AXUIElementSetAttributeValue(selection.1, kAXSelectedAttribute as CFString, kCFBooleanTrue)
        if selectedStatus != .success {
            let focused = AXUIElementSetAttributeValue(selection.0, kAXFocusedAttribute as CFString, kCFBooleanTrue)
            guard focused == .success else {
                print("could not focus the fixture list: \(focused)")
                exit(1)
            }
            for down in [true, false] {
                let event = CGEvent(keyboardEventSource: nil, virtualKey: 125, keyDown: down)!
                event.postToPid(pid)
            }
        }
    }
    _ = wait("selected application") {
        let selected = value(selection.0, kAXSelectedChildrenAttribute as String) as? [AXUIElement] ?? []
        return selected.contains(where: { CFEqual($0, selection.1) }) ? true : nil
    }
    _ = wait("Open action") { press("Open", attribute: kAXDefaultButtonAttribute as String, in: panel) ? true : nil }
}

let open = wait("application picker") { picker() }
selectApplication(in: open)
print("selected the disposable .app bundle")
_ = wait("first picker to close") { picker() == nil ? true : nil }
let openWith = wait("Open With picker", timeout: 60) { picker() }
selectApplication(in: openWith)
print("selected the disposable .app bundle for Open With")
_ = wait("Open With picker to close") { picker() == nil ? true : nil }
let cancelledOpenWith = wait("second Open With picker", timeout: 60) { picker() }
_ = wait("Open With Cancel action") { press("Cancel", attribute: kAXCancelButtonAttribute as String, in: cancelledOpenWith) ? true : nil }
print("cancelled Open With")
_ = wait("second Open With picker to close") { picker() == nil ? true : nil }
let defaultOpenWith = wait("default Open With picker", timeout: 60) { picker() }
_ = wait("default Open With Cancel action") { press("Cancel", attribute: kAXCancelButtonAttribute as String, in: defaultOpenWith) ? true : nil }
print("cancelled the default Open With picker")
_ = wait("default Open With picker to close") { picker() == nil ? true : nil }
let automaticImage = wait("automatic image picker", timeout: 60) { picker() }
selectApplication(in: automaticImage)
print("selected the application for automatic image sharing")
_ = wait("automatic image picker to close") { picker() == nil ? true : nil }
let automaticVideo = wait("automatic recording picker", timeout: 60) { picker() }
selectApplication(in: automaticVideo)
print("selected the application for automatic recording sharing")
_ = wait("automatic recording picker to close") { picker() == nil ? true : nil }
let cancelledImage = wait("cancellable automatic image picker", timeout: 60) { picker() }
_ = wait("automatic image Cancel action") { press("Cancel", attribute: kAXCancelButtonAttribute as String, in: cancelledImage) ? true : nil }
print("cancelled automatic image sharing")
_ = wait("cancellable automatic image picker to close") { picker() == nil ? true : nil }
let cancelledVideo = wait("cancellable automatic recording picker", timeout: 60) { picker() }
_ = wait("automatic recording Cancel action") { press("Cancel", attribute: kAXCancelButtonAttribute as String, in: cancelledVideo) ? true : nil }
print("cancelled automatic recording sharing")
_ = wait("cancellable automatic recording picker to close") { picker() == nil ? true : nil }
let second = wait("second application picker", timeout: 60) { picker() }
_ = wait("Cancel action") { press("Cancel", attribute: kAXCancelButtonAttribute as String, in: second) ? true : nil }
print("cancelled the second picker")
_ = wait("second picker to close") { picker() == nil ? true : nil }
func folderPicker() -> AXUIElement? {
    let windows = value(app, kAXWindowsAttribute as String) as? [AXUIElement] ?? []
    for window in windows {
        if (value(window, kAXTitleAttribute as String) as? String)?.lowercased() == "share to folder" {
            return window
        }
        let children = value(window, kAXChildrenAttribute as String) as? [AXUIElement] ?? []
        for sheet in children where (value(sheet, kAXRoleAttribute as String) as? String) == "AXSheet" {
            let title = value(sheet, kAXTitleAttribute as String) as? String ?? ""
            let description = value(sheet, kAXDescriptionAttribute as String) as? String ?? ""
            if title.lowercased() == "share to folder" || description.lowercased() == "share to folder" {
                return sheet
            }
        }
    }
    return nil
}
let folder = wait("folder picker", timeout: 60) { folderPicker() }
_ = wait("folder Cancel action") { press("Cancel", attribute: kAXCancelButtonAttribute as String, in: folder) ? true : nil }
print("cancelled the folder picker")
_ = wait("folder picker to close") { folderPicker() == nil ? true : nil }
_ = wait("second folder picker", timeout: 60) { folderPicker() }
let destination = wait("fixture destination", timeout: 15) { () -> (AXUIElement, AXUIElement, AXUIElement)? in
    guard let current = folderPicker() else { return nil }
    guard let name = firstElement(current, matching: { (value($0, kAXValueAttribute as String) as? String) == folderName }),
          let parent = value(name, kAXParentAttribute as String), CFGetTypeID(parent) == AXUIElementGetTypeID() else { return nil }
    let group = parent as! AXUIElement
    guard (value(group, kAXRoleAttribute as String) as? String) == "AXGroup",
          let parent = value(group, kAXParentAttribute as String), CFGetTypeID(parent) == AXUIElementGetTypeID() else { return nil }
    let list = parent as! AXUIElement
    guard (value(list, kAXRoleAttribute as String) as? String) == "AXList" else { return nil }
    return (current, list, group)
}
guard AXUIElementSetAttributeValue(destination.1, kAXSelectedChildrenAttribute as CFString, [destination.2] as CFArray) == .success else {
    print("could not select the fixture destination")
    exit(1)
}
_ = wait("selected folder") {
    let selected = value(destination.1, kAXSelectedChildrenAttribute as String) as? [AXUIElement] ?? []
    return selected.contains(where: { CFEqual($0, destination.2) }) ? true : nil
}
_ = wait("folder Open action") { press("Open", attribute: kAXDefaultButtonAttribute as String, in: destination.0) ? true : nil }
print("selected the fixture destination")
