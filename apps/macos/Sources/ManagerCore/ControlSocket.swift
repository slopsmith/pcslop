import Darwin
import Foundation

public struct ControlError: Error, LocalizedError, Sendable, Equatable {
    public var message: String
    public var errorDescription: String? { message }

    public init(message: String) {
        self.message = message
    }
}

// pcslop answers each command line with the answer's lines and then a line holding a single `.`.
public struct ReplyFramer: Sendable {
    private var buffer = Data()
    private static let terminator = Data("\n.\n".utf8)

    public init() {}

    public mutating func append(_ chunk: Data) {
        buffer.append(chunk)
    }

    public mutating func nextReply() -> String? {
        guard let range = buffer.firstRange(of: Self.terminator) else { return nil }
        let reply = String(decoding: buffer[buffer.startIndex..<range.lowerBound], as: UTF8.self)
        buffer.removeSubrange(buffer.startIndex..<range.upperBound)
        return reply
    }
}

public enum ControlSocket {
    public static func send(_ command: String, to path: String, timeout: TimeInterval = 15) throws(ControlError)
        -> String
    {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw ControlError(message: "cannot make a socket") }
        defer { close(fd) }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(path.utf8)
        guard pathBytes.count < MemoryLayout.size(ofValue: addr.sun_path) else {
            throw ControlError(message: "the socket path is too long")
        }
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            raw.copyBytes(from: pathBytes)
            raw[pathBytes.count] = 0
        }
        let len = socklen_t(MemoryLayout<sockaddr_un>.size)
        let connected = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, len) }
        }
        guard connected == 0 else { throw ControlError(message: "the machine is not listening") }

        var tv = timeval(tv_sec: Int(timeout), tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        var nosigpipe: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &nosigpipe, socklen_t(MemoryLayout<Int32>.size))

        let line = Array((command + "\n").utf8)
        let written = line.withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
        guard written == line.count else { throw ControlError(message: "cannot send the command") }

        var framer = ReplyFramer()
        var chunk = [UInt8](repeating: 0, count: 4096)
        while true {
            if let reply = framer.nextReply() { return reply }
            let n = chunk.withUnsafeMutableBytes { read(fd, $0.baseAddress, $0.count) }
            if n > 0 {
                framer.append(Data(chunk[0..<n]))
            } else if n == 0 {
                throw ControlError(message: "the machine closed the connection")
            } else if errno == EINTR {
                continue
            } else {
                throw ControlError(message: "no answer from the machine")
            }
        }
    }

    public static func send(_ command: String, to path: String, timeout: TimeInterval = 15) async throws(ControlError)
        -> String
    {
        let result: Result<String, ControlError> = await withCheckedContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                continuation.resume(
                    returning: Result { () throws(ControlError) in
                        try send(command, to: path, timeout: timeout)
                    })
            }
        }
        return try result.get()
    }
}

// The first line of `status`, e.g.
// `time 12.3s instructions 456 (400 translated) mips 78.9 mode jit cpl 0 halted false paused false`.
public struct StatusReport: Sendable, Hashable {
    public var uptime: Double
    public var instructions: UInt64
    public var translated: UInt64
    public var mips: Double
    public var mode: String
    public var halted: Bool
    public var paused: Bool
    public var details: String

    public init?(_ text: String) {
        let lines = text.split(separator: "\n", maxSplits: 1, omittingEmptySubsequences: false)
        guard let first = lines.first else { return nil }
        let words = first.split(separator: " ").map(String.init)
        func value(after key: String) -> String? {
            guard let i = words.firstIndex(of: key), i + 1 < words.count else { return nil }
            return words[i + 1]
        }
        guard let time = value(after: "time"), time.hasSuffix("s"), let uptime = Double(time.dropLast()),
            let instructions = value(after: "instructions").flatMap({ UInt64($0) }),
            let mips = value(after: "mips").flatMap({ Double($0) }),
            let mode = value(after: "mode")
        else { return nil }
        self.uptime = uptime
        self.instructions = instructions
        self.translated = words.first { $0.hasPrefix("(") }.flatMap { UInt64($0.dropFirst()) } ?? 0
        self.mips = mips
        self.mode = mode
        self.halted = value(after: "halted") == "true"
        self.paused = value(after: "paused") == "true"
        self.details = lines.count > 1 ? String(lines[1]) : ""
    }
}
