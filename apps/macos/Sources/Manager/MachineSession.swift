import Foundation
import ManagerCore
import Observation
import os

struct LaunchSettings {
    var binary: URL
    var firmware: Firmware
}

// One pcslop process: its output, its control socket and the last status it reported.
@MainActor
@Observable
final class MachineSession {
    let machineID: Machine.ID
    private(set) var log = LogBuffer(capacity: 5000)
    private(set) var startedAt: Date?
    private(set) var isPaused = false
    private(set) var status: StatusReport?
    private(set) var cdrom: String?
    private(set) var busy: String?
    var isRunning: Bool { process != nil }

    private var process: Process?
    private var controlPath: String?
    private let pending = OSAllocatedUnfairLock(initialState: Data())
    private var drainTask: Task<Void, Never>?
    private var pollTask: Task<Void, Never>?
    private var thumbnailTask: Task<Void, Never>?
    var onExit: (() -> Void)?
    var onThumbnail: (() -> Void)?

    static let thumbnailInterval = Duration.seconds(10)

    init(machineID: Machine.ID) {
        self.machineID = machineID
    }

    func launch(_ machine: Machine, settings: LaunchSettings, folder: URL, thumbnail: URL, load: SavedState?) throws {
        guard process == nil else { return }
        let control = LaunchCommand.socketPath(id: UUID().uuidString)
        let args = try LaunchCommand.arguments(
            for: machine.config, firmware: settings.firmware, control: control, load: load?.folder)

        let child = Process()
        child.executableURL = settings.binary
        child.arguments = args
        child.currentDirectoryURL = folder
        child.standardInput = FileHandle.nullDevice
        let pipe = Pipe()
        child.standardOutput = pipe
        child.standardError = pipe
        let pending = self.pending
        pipe.fileHandleForReading.readabilityHandler = { handle in
            let data = handle.availableData
            if data.isEmpty {
                handle.readabilityHandler = nil
            } else {
                pending.withLock { $0.append(data) }
            }
        }
        child.terminationHandler = { [weak self] proc in
            let status = proc.terminationStatus
            let reason = proc.terminationReason
            Task { @MainActor in self?.finished(status: status, reason: reason) }
        }

        log.clear()
        log.appendLine("$ \(settings.binary.path) \(args.map(Self.quoted).joined(separator: " "))")
        do {
            try child.run()
        } catch {
            pipe.fileHandleForReading.readabilityHandler = nil
            throw error
        }
        process = child
        controlPath = control
        startedAt = .now
        isPaused = false
        status = nil
        cdrom = load == nil ? machine.config.cdrom : nil
        drainTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .milliseconds(150))
                self?.drain()
            }
        }
        pollTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(1))
            await self?.refreshCD()
            while !Task.isCancelled {
                await self?.refreshStatus()
                try? await Task.sleep(for: .seconds(2))
            }
        }
        thumbnailTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.thumbnailInterval)
                guard let self, !Task.isCancelled else { return }
                if !isPaused, busy == nil, case .success = await send("shot \(thumbnail.path)", timeout: 5) {
                    onThumbnail?()
                }
            }
        }
    }

    @discardableResult
    func send(_ command: String, timeout: TimeInterval = 15) async -> Result<String, ControlError> {
        guard let path = controlPath else { return .failure(ControlError(message: "the machine is not running")) }
        do {
            return .success(try await ControlSocket.send(command, to: path, timeout: timeout))
        } catch {
            return .failure(error)
        }
    }

    func perform(_ command: String) async {
        log.appendLine("> \(command)")
        switch await send(command) {
        case .success(let reply): if !reply.isEmpty { log.appendLine(reply) }
        case .failure(let error): log.appendLine("control: \(error.message)")
        }
    }

    func setPaused(_ paused: Bool) async {
        if case .success = await send(paused ? "pause" : "resume") { isPaused = paused }
    }

    func changeCD(_ iso: String?) async {
        await perform(iso.map { "cdrom \($0)" } ?? "eject")
        await refreshCD()
    }

    func saveState(into folder: URL) async -> Bool {
        busy = "Saving state"
        defer { busy = nil }
        log.appendLine("> save \(folder.path)")
        switch await send("save \(folder.path)", timeout: 130) {
        case .success(let reply):
            log.appendLine(reply)
            guard reply.hasPrefix("saved ") else { return false }
            await send("shot \(folder.appendingPathComponent(MachineStore.thumbnailFile).path)", timeout: 5)
            return true
        case .failure(let error):
            log.appendLine("control: \(error.message)")
            return false
        }
    }

    func restore(_ state: SavedState) async {
        busy = "Restoring state"
        defer { busy = nil }
        log.appendLine("> load \(state.folder.path)")
        switch await send("load \(state.folder.path)", timeout: 130) {
        case .success(let reply): log.appendLine(reply)
        case .failure(let error): log.appendLine("control: \(error.message)")
        }
        await refreshCD()
    }

    // Takes a last screenshot for the library, asks the machine to quit, and
    // sends SIGTERM if that fails or it is still running a few seconds later.
    func stop(thumbnail: URL?) {
        guard let child = process else { return }
        let pid = child.processIdentifier
        let path = controlPath
        busy = "Stopping"
        Task.detached {
            if let path, let thumbnail {
                _ = try? await ControlSocket.send("shot \(thumbnail.path)", to: path, timeout: 3)
                try? await Task.sleep(for: .milliseconds(700))
            }
            var asked = false
            if let path { asked = (try? await ControlSocket.send("quit", to: path, timeout: 3)) != nil }
            if asked {
                for _ in 0..<30 where child.isRunning {
                    try? await Task.sleep(for: .milliseconds(100))
                }
            }
            if child.isRunning, child.processIdentifier == pid {
                kill(pid, SIGTERM)
            }
        }
    }

    func terminate() {
        guard let child = process else { return }
        Darwin.kill(child.processIdentifier, SIGTERM)
    }

    func forceQuit() {
        guard let child = process else { return }
        Darwin.kill(child.processIdentifier, SIGKILL)
    }

    func clearLog() {
        log.clear()
    }

    private func refreshStatus() async {
        guard case .success(let reply) = await send("status", timeout: 5), let report = StatusReport(reply) else {
            return
        }
        status = report
        isPaused = report.paused
    }

    private func refreshCD() async {
        guard case .success(let reply) = await send("cdrom", timeout: 5) else { return }
        cdrom = reply == "empty" ? nil : reply
    }

    private func drain() {
        let data = pending.withLock { buffer in
            defer { buffer.removeAll() }
            return buffer
        }
        if !data.isEmpty { log.append(data) }
    }

    private func finished(status code: Int32, reason: Process.TerminationReason) {
        drainTask?.cancel()
        drainTask = nil
        pollTask?.cancel()
        pollTask = nil
        thumbnailTask?.cancel()
        thumbnailTask = nil
        drain()
        let how = reason == .uncaughtSignal ? "killed by signal \(code)" : "exited with status \(code)"
        Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(300))
            guard let self else { return }
            drain()
            log.flush()
            log.appendLine("pcslop \(how)")
        }
        process = nil
        controlPath = nil
        startedAt = nil
        status = nil
        isPaused = false
        busy = nil
        onExit?()
    }

    private static func quoted(_ arg: String) -> String {
        arg.contains(where: { $0.isWhitespace || $0 == "'" || $0 == "\"" })
            ? "'" + arg.replacingOccurrences(of: "'", with: #"'\''"#) + "'" : arg
    }
}

@MainActor
@Observable
final class Sessions {
    private(set) var all: [Machine.ID: MachineSession] = [:]
    var lastError: String?
    var onExit: ((Machine.ID) -> Void)?
    var onThumbnail: ((Machine.ID) -> Void)?

    var runningCount: Int { all.values.filter(\.isRunning).count }

    func session(for id: Machine.ID) -> MachineSession? { all[id] }

    func isRunning(_ id: Machine.ID) -> Bool { all[id]?.isRunning ?? false }

    func start(_ machine: Machine, settings: LaunchSettings?, folder: URL, load: SavedState? = nil) {
        guard !isRunning(machine.id) else { return }
        guard let settings else {
            lastError =
                "pcslop or its BIOS was not found. Set them in Settings, set PCSLOP_BIN, or build target/release/pcslop."
            return
        }
        let problems = machine.config.problems(radeonROM: appRadeonROM())
        if load == nil, !problems.isEmpty {
            lastError = problems.joined(separator: "\n")
            return
        }
        let session = all[machine.id] ?? MachineSession(machineID: machine.id)
        let id = machine.id
        session.onExit = { [weak self] in self?.onExit?(id) }
        session.onThumbnail = { [weak self] in self?.onThumbnail?(id) }
        all[machine.id] = session
        do {
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            try session.launch(
                machine, settings: settings, folder: folder,
                thumbnail: folder.appendingPathComponent(MachineStore.thumbnailFile), load: load)
        } catch let error as ShellWordsError {
            lastError = "Extra arguments: \(error.message)"
        } catch {
            lastError = "Cannot start pcslop: \(error.localizedDescription)"
        }
    }

    func terminateAll() {
        for session in all.values { session.terminate() }
    }
}
