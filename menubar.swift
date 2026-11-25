import Cocoa

class AppDelegate: NSObject, NSApplicationDelegate {
    var statusItem: NSStatusItem!
    var timer: Timer?
    let repoDir: String
    let switcherPath: String

    override init() {
        let binDir = (ProcessInfo.processInfo.arguments[0] as NSString).deletingLastPathComponent
        self.repoDir = binDir.isEmpty ? FileManager.default.currentDirectoryPath : binDir
        self.switcherPath = "\(self.repoDir)/claude-switch.mjs"
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)

        // Load icon from .icns file next to binary
        let iconPath = (ProcessInfo.processInfo.arguments[0] as NSString)
            .deletingLastPathComponent + "/OneDark.icns"
        if let img = NSImage(contentsOfFile: iconPath) {
            img.size = NSSize(width: 18, height: 18)
            statusItem.button?.image = img
            statusItem.button?.title = ""
        } else {
            statusItem.button?.title = "🧠"
        }

        updateMenu()
        // Refresh every 10s
        timer = Timer.scheduledTimer(withTimeInterval: 10, repeats: true) { [weak self] _ in
            self?.updateMenu()
        }
    }

    func getProfiles() -> (active: String, profiles: [(key: String, name: String, url: String)])? {
        let profilesPath = "\(repoDir)/profiles.json"
        guard let data = FileManager.default.contents(atPath: profilesPath),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let active = json["active"] as? String,
              let profiles = json["profiles"] as? [String: Any] else { return nil }

        var result: [(key: String, name: String, url: String)] = []
        for (key, value) in profiles {
            guard let p = value as? [String: Any],
                  let name = p["name"] as? String,
                  let env = p["env"] as? [String: String] else { continue }
            result.append((key: key, name: name, url: env["ANTHROPIC_BASE_URL"] ?? "default"))
        }
        result.sort { $0.key < $1.key }
        return (active: active, profiles: result)
    }

    func updateMenu() {
        guard let info = getProfiles() else { return }

        let menu = NSMenu()

        // Profile list with checkmark for active
        for p in info.profiles {
            let item = NSMenuItem(title: "\(p.name)",
                                  action: #selector(switchProfile(_:)),
                                  keyEquivalent: "")
            item.target = self
            item.representedObject = p.key
            if p.key == info.active {
                item.state = .on
            }
            menu.addItem(item)
        }

        menu.addItem(NSMenuItem.separator())
        menu.addItem(NSMenuItem(title: "Quit", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q"))

        statusItem.menu = menu
    }

    func shortUrl(_ url: String) -> String {
        url.replacingOccurrences(of: "https://", with: "")
           .replacingOccurrences(of: "http://", with: "")
           .components(separatedBy: "/").first ?? url
    }

    @objc func switchProfile(_ sender: NSMenuItem) {
        guard let key = sender.representedObject as? String else { return }
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        proc.arguments = ["node", switcherPath, "use", key]
        try? proc.run()
        proc.waitUntilExit()
        updateMenu()

        // Notification
        let notif = NSUserNotification()
        notif.title = "Claude Gateway"
        notif.informativeText = "Switched to: \(key)"
        NSUserNotificationCenter.default.deliver(notif)
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory) // No dock icon
let delegate = AppDelegate()
app.delegate = delegate
app.run()