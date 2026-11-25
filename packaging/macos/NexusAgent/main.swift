import AppKit
import Security
import UniformTypeIdentifiers

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private let port = 8471
    private let keychainService = "dev.nexus-agent.desktop"
    private let keychainAccount = "server-token"
    private var statusItem: NSStatusItem!
    private var server: Process?
    private var logHandle: FileHandle?
    private var restarting = false
    private var choosingProject = false
    private var openedOnLaunch = false
    private var serverToken = ""
    private var serverExecutable: URL {
        Bundle.main.bundleURL.appendingPathComponent("Contents/Resources/nexus-server")
    }
    private var logURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Logs/Nexus Agent/server.log")
    }
    private var supportURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/Nexus Agent", isDirectory: true)
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        statusItem.button?.image = NSImage(
            systemSymbolName: "sparkle",
            accessibilityDescription: "Nexus Agent"
        )
        rebuildMenu()
        startServer()
    }

    func applicationWillTerminate(_ notification: Notification) {
        server?.terminate()
        serverToken = ""
        try? logHandle?.close()
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        openInterface()
        return true
    }

    func application(_ application: NSApplication, open urls: [URL]) {
        if urls.contains(where: { $0.absoluteString == "nexus-agent://open-project" }) {
            chooseProject()
        }
    }

    private func rebuildMenu() {
        let menu = NSMenu()
        let status = NSMenuItem(title: server?.isRunning == true ? "Service Running" : "Service Stopped", action: nil, keyEquivalent: "")
        status.isEnabled = false
        menu.addItem(status)
        menu.addItem(.separator())
        menu.addItem(item("Open Nexus", #selector(openInterface)))
        menu.addItem(item("Open in Default Browser", #selector(openInDefaultBrowser)))
        menu.addItem(item("Open Project…", #selector(chooseProject)))
        menu.addItem(item("Restart Service", #selector(restartServer)))
        menu.addItem(item("Choose Installed Web App…", #selector(chooseWebApp)))
        if UserDefaults.standard.string(forKey: "selectedWebApp") != nil {
            menu.addItem(item("Forget Installed Web App", #selector(forgetWebApp)))
        }
        menu.addItem(item("View Service Log", #selector(showLog)))
        menu.addItem(.separator())
        menu.addItem(item("Quit Nexus", #selector(quit)))
        statusItem.menu = menu
        statusItem.button?.toolTip = server?.isRunning == true ? "Nexus Agent is running" : "Nexus Agent is stopped"
    }

    private func item(_ title: String, _ action: Selector) -> NSMenuItem {
        let result = NSMenuItem(title: title, action: action, keyEquivalent: "")
        result.target = self
        return result
    }

    private func startServer(preferWebApp: Bool = true) {
        guard server == nil else { return }
        do {
            try FileManager.default.createDirectory(at: supportURL, withIntermediateDirectories: true)
            try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: supportURL.path)
            try FileManager.default.createDirectory(
                at: logURL.deletingLastPathComponent(),
                withIntermediateDirectories: true
            )
            if !FileManager.default.fileExists(atPath: logURL.path) {
                FileManager.default.createFile(atPath: logURL.path, contents: nil)
            }
            try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: logURL.path)
            logHandle = try FileHandle(forWritingTo: logURL)
            try logHandle?.seekToEnd()

            let token = try keychainToken()
            serverToken = token
            let process = Process()
            process.executableURL = serverExecutable
            var arguments = [
                "--port", String(port),
                "--auth-token-env", "NEXUS_DESKTOP_SERVER_TOKEN",
                "--session-store", supportURL.appendingPathComponent("sessions").path,
                "--execution", "local",
            ]
            if let root = UserDefaults.standard.string(forKey: "projectRoot") {
                arguments += ["--tools", "real", "--tools-root", root]
            } else {
                arguments += ["--tools", "fake"]
            }
            process.arguments = arguments
            var environment = ProcessInfo.processInfo.environment
            environment["NEXUS_DESKTOP_SERVER_TOKEN"] = token
            environment["NEXUS_DESKTOP_LAUNCHER"] = "1"
            // Finder does not inherit the terminal's Homebrew PATH. Apple's
            // LibreSSL s_client lacks the hostname verification option.
            if environment["NEXUS_OPENSSL_EXECUTABLE"] == nil,
               let openssl = ["/opt/homebrew/opt/openssl@3/bin/openssl", "/opt/homebrew/bin/openssl"]
                   .first(where: { FileManager.default.isExecutableFile(atPath: $0) }) {
                environment["NEXUS_OPENSSL_EXECUTABLE"] = openssl
            }
            process.environment = environment
            process.standardOutput = logHandle
            process.standardError = logHandle
            process.terminationHandler = { [weak self] process in
                Task { @MainActor in
                    guard let self else { return }
                    self.server = nil
                    self.rebuildMenu()
                }
            }
            try process.run()
            server = process
            rebuildMenu()
            Task { await waitUntilReadyAndOpen(process, preferWebApp: preferWebApp) }
        } catch {
            server = nil
            rebuildMenu()
            showError("Nexus Service Failed to Start", detail: error.localizedDescription)
        }
    }

    private func waitUntilReadyAndOpen(_ process: Process, preferWebApp: Bool) async {
        let healthURL = URL(string: "http://127.0.0.1:\(port)/health")!
        for _ in 0..<100 {
            guard server === process, process.isRunning, !restarting else { return }
            do {
                let (_, response) = try await URLSession.shared.data(from: healthURL)
                if (response as? HTTPURLResponse)?.statusCode == 200,
                   server === process, process.isRunning, !restarting {
                    if !openedOnLaunch {
                        openedOnLaunch = true
                        await openInterfaceWithLogin(preferWebApp: preferWebApp)
                    }
                    return
                }
            } catch {
                // The local listener may not be ready yet.
            }
            try? await Task.sleep(for: .milliseconds(200))
        }
        showError("Nexus Service Did Not Become Ready", detail: "Check the service log from the menu bar, then try again.")
    }

    @objc private func openInterface() {
        Task { await openInterfaceWithLogin(preferWebApp: true) }
    }

    @objc private func openInDefaultBrowser() {
        Task { await openInterfaceWithLogin(preferWebApp: false) }
    }

    private func openInterfaceWithLogin(preferWebApp: Bool) async {
        guard !restarting else { return }
        guard server?.isRunning == true else {
            openedOnLaunch = false
            startServer(preferWebApp: preferWebApp)
            return
        }
        let code: String
        do {
            code = try await issueLoginCode()
        } catch {
            showError("Could Not Open Nexus", detail: "The local service could not prepare automatic sign-in. Try again or view the service log.")
            return
        }
        var components = URLComponents(string: "http://127.0.0.1:\(port)/")!
        components.fragment = "nexus-login=\(code)"
        guard let url = components.url else { return }

        if preferWebApp, let selected = UserDefaults.standard.string(forKey: "selectedWebApp") {
            if FileManager.default.fileExists(atPath: selected) {
                do {
                    _ = try await NSWorkspace.shared.open(
                        [url],
                        withApplicationAt: URL(fileURLWithPath: selected),
                        configuration: NSWorkspace.OpenConfiguration()
                    )
                    return
                } catch {
                    // Fall back to the default browser if the selected app cannot be opened.
                }
            }
            if UserDefaults.standard.string(forKey: "selectedWebApp") == selected {
                forgetWebApp()
            }
        }
        if !NSWorkspace.shared.open(url) {
            showError("Could Not Open Nexus", detail: "macOS could not open the Nexus page in your default browser.")
        }
    }

    private func issueLoginCode() async throws -> String {
        let code = try randomHex()
        var request = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/desktop-login/issue")!)
        request.httpMethod = "POST"
        request.setValue("Bearer \(serverToken)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: ["code": code])
        request.timeoutInterval = 5
        let (_, response) = try await URLSession.shared.data(for: request)
        guard (response as? HTTPURLResponse)?.statusCode == 204 else {
            throw NSError(domain: "NexusDesktop", code: 1)
        }
        return code
    }

    @objc private func restartServer() {
        restartService(project: nil)
    }

    private func restartService(project: URL?) {
        guard !restarting else { return }
        restarting = true
        let process = server
        if process?.isRunning == true { process?.terminate() }
        let port = self.port
        Task {
            // Let the managed child's termination handler clear it first.
            for _ in 0..<50 {
                if server == nil { break }
                try? await Task.sleep(for: .milliseconds(100))
            }
            guard server == nil else {
                restarting = false
                showError("Could Not Restart Nexus", detail: "The previous service did not stop. Check the service log.")
                return
            }
            let failure = await Task.detached { Self.releasePort(port) }.value
            restarting = false
            if let failure {
                showError("Could Not Restart Nexus", detail: failure)
                return
            }
            if let project {
                UserDefaults.standard.set(project.path, forKey: "projectRoot")
                openedOnLaunch = false
            }
            startServer()
        }
    }

    @objc private func chooseProject() {
        guard !choosingProject, !restarting else { return }
        choosingProject = true
        defer { choosingProject = false }
        NSApp.activate(ignoringOtherApps: true)
        let picker = NSOpenPanel()
        picker.title = "Open Project"
        picker.message = "Choose a project folder to enable local tools for it. Restarting Nexus interrupts any running tasks."
        picker.prompt = "Open and Restart"
        picker.canChooseDirectories = true
        picker.canChooseFiles = false
        guard picker.runModal() == .OK, let folder = picker.url else { return }

        let validator = Process()
        validator.executableURL = serverExecutable
        validator.arguments = ["--check-project-root", folder.path]
        validator.standardError = FileHandle.nullDevice
        do {
            try validator.run()
            validator.waitUntilExit()
            guard validator.terminationStatus == 0 else {
                showError("Could Not Open Project", detail: "Choose an accessible project folder, not your home or a protected directory.")
                return
            }
            restartService(project: folder)
        } catch {
            showError("Could Not Validate Project", detail: error.localizedDescription)
        }
    }

    nonisolated private static func releasePort(_ port: Int) -> String? {
        func lsof(_ arguments: [String]) -> (Int32, String)? {
            let process = Process()
            let output = Pipe()
            process.executableURL = URL(fileURLWithPath: "/usr/sbin/lsof")
            process.arguments = arguments
            process.standardOutput = output
            process.standardError = FileHandle.nullDevice
            do { try process.run() } catch { return nil }
            let data = output.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            return (process.terminationStatus, String(decoding: data, as: UTF8.self))
        }

        func listeners() -> [String]? {
            guard let (status, output) = lsof(["-nP", "-iTCP:\(port)", "-sTCP:LISTEN", "-Fpun"])
            else { return nil }
            if status == 1, output.isEmpty { return [] }
            return status == 0 ? output.split(separator: "\n").map(String.init) : nil
        }

        guard let entries = listeners() else { return "Could not check whether TCP port \(port) is in use." }
        if entries.isEmpty { return nil }
        let pids = entries.filter { $0.hasPrefix("p") }
        let users = entries.filter { $0.hasPrefix("u") }
        let sockets = entries.filter { $0.hasPrefix("n") }
        let conflict = "TCP port \(port) is in use by another service. That process was left running."
        guard pids.count == 1, let pid = Int32(pids[0].dropFirst()), pid > 1,
              users == ["u\(getuid())"], sockets == ["n127.0.0.1:\(port)"],
              let (status, files) = lsof(["-a", "-p", String(pid), "-d", "txt", "-Fn"]),
              status == 0,
              let executable = files.split(separator: "\n").first(where: { $0.hasPrefix("n") }),
              URL(fileURLWithPath: String(executable.dropFirst())).lastPathComponent == "nexus-server",
              listeners() == entries
        else { return conflict }
        guard kill(pid, SIGTERM) == 0 || errno == ESRCH else {
            return "Could not stop the previous Nexus service using TCP port \(port)."
        }
        for _ in 0..<50 {
            if listeners()?.isEmpty == true { return nil }
            Thread.sleep(forTimeInterval: 0.1)
        }
        return "TCP port \(port) was not released. A new service was not started."
    }

    @objc private func chooseWebApp() {
        NSApp.activate(ignoringOtherApps: true)
        let panel = NSOpenPanel()
        panel.title = "Choose an Installed Nexus Web App"
        panel.message = "Choose the app installed for the Nexus website, not a regular browser application."
        panel.prompt = "Choose"
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowedContentTypes = [.application]
        if panel.runModal() == .OK, let url = panel.url {
            UserDefaults.standard.set(url.path, forKey: "selectedWebApp")
            rebuildMenu()
            openInterface()
        }
    }

    @objc private func forgetWebApp() {
        UserDefaults.standard.removeObject(forKey: "selectedWebApp")
        rebuildMenu()
    }

    @objc private func showLog() {
        NSWorkspace.shared.open(logURL)
    }

    @objc private func quit() {
        NSApp.terminate(nil)
    }

    private func keychainToken() throws -> String {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: keychainService,
            kSecAttrAccount as String: keychainAccount,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecSuccess, let data = result as? Data,
           let token = String(data: data, encoding: .utf8) {
            guard token.utf8.count == 64,
                  token.utf8.allSatisfy({ (48...57).contains($0) || (65...70).contains($0) || (97...102).contains($0) })
            else {
                throw NSError(domain: "NexusDesktop", code: 2)
            }
            return token
        }
        guard status == errSecItemNotFound else {
            throw NSError(domain: NSOSStatusErrorDomain, code: Int(status))
        }
        let token = try randomHex()
        let add: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: keychainService,
            kSecAttrAccount as String: keychainAccount,
            kSecValueData as String: Data(token.utf8),
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
        ]
        let addStatus = SecItemAdd(add as CFDictionary, nil)
        guard addStatus == errSecSuccess else {
            throw NSError(domain: NSOSStatusErrorDomain, code: Int(addStatus))
        }
        return token
    }

    private func randomHex() throws -> String {
        var bytes = [UInt8](repeating: 0, count: 32)
        let status = SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes)
        guard status == errSecSuccess else {
            throw NSError(domain: NSOSStatusErrorDomain, code: Int(status))
        }
        return bytes.map { String(format: "%02x", $0) }.joined()
    }

    private func showError(_ title: String, detail: String) {
        let alert = NSAlert()
        alert.messageText = title
        alert.informativeText = detail
        alert.alertStyle = .warning
        alert.addButton(withTitle: "OK")
        alert.runModal()
    }
}

@main
struct NexusAgentApp {
    @MainActor
    static func main() {
        let application = NSApplication.shared
        let delegate = AppDelegate()
        application.delegate = delegate
        withExtendedLifetime(delegate) { application.run() }
    }
}
