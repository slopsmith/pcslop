import Darwin
import Foundation
import Testing

@testable import ManagerCore

struct ReplyFramerTests {
    @Test func waitsForTheDotLine() {
        var framer = ReplyFramer()
        framer.append(Data("paused\n".utf8))
        #expect(framer.nextReply() == nil)
        framer.append(Data(".\n".utf8))
        #expect(framer.nextReply() == "paused")
    }

    @Test func keepsMultiLineAnswersWhole() {
        var framer = ReplyFramer()
        framer.append(Data("one\ntwo\n.\n".utf8))
        #expect(framer.nextReply() == "one\ntwo")
    }

    @Test func readsAnEmptyAnswer() {
        var framer = ReplyFramer()
        framer.append(Data("\n.\n".utf8))
        #expect(framer.nextReply() == "")
    }

    @Test func splitsBackToBackReplies() {
        var framer = ReplyFramer()
        framer.append(Data("ok\n.\nresu".utf8))
        #expect(framer.nextReply() == "ok")
        #expect(framer.nextReply() == nil)
        framer.append(Data("med\n.\n".utf8))
        #expect(framer.nextReply() == "resumed")
    }
}

struct StatusReportTests {
    @Test func readsTheSummaryLine() throws {
        let text = """
            time 12.5s instructions 4567 (4000 translated) mips 78.9 mode jit cpl 0 halted false paused true
            eax=00000000 ecx=00000001
            host: jit 90% interp 5% halted 5% over 1.0s
            """
        let report = try #require(StatusReport(text))
        #expect(report.uptime == 12.5)
        #expect(report.instructions == 4567)
        #expect(report.translated == 4000)
        #expect(report.mips == 78.9)
        #expect(report.mode == "jit")
        #expect(!report.halted)
        #expect(report.paused)
        #expect(report.details.hasPrefix("eax="))
    }

    @Test(arguments: ["", "no answer", "the machine is gone", "time abc instructions 1 mips 2 mode jit"])
    func rejectsOtherAnswers(text: String) {
        #expect(StatusReport(text) == nil)
    }
}

struct ControlSocketTests {
    @Test func sendsALineAndReadsTheFramedAnswer() async throws {
        let path = LaunchCommand.socketPath(id: UUID().uuidString)
        let server = try FakeServer(path: path) { line in line == "status" ? "a\nb" : "?" }
        defer { server.stop() }
        #expect(try await ControlSocket.send("status", to: path) == "a\nb")
    }

    @Test func missingSocketThrows() {
        #expect(throws: ControlError.self) {
            try ControlSocket.send("status", to: "/tmp/pcslop-no-such-\(UUID().uuidString.prefix(8)).sock")
        }
    }
}

// Answers one connection the way pcslop's control server does.
private final class FakeServer: Sendable {
    private let fd: Int32
    private let path: String

    init(path: String, answer: @escaping @Sendable (String) -> String) throws {
        self.path = path
        unlink(path)
        fd = socket(AF_UNIX, SOCK_STREAM, 0)
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            let bytes = Array(path.utf8)
            raw.copyBytes(from: bytes)
            raw[bytes.count] = 0
        }
        let bound = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard bound == 0, listen(fd, 1) == 0 else { throw ControlError(message: "bind failed") }
        let fd = self.fd
        Thread.detachNewThread {
            let conn = accept(fd, nil, nil)
            guard conn >= 0 else { return }
            defer { close(conn) }
            var line = Data()
            var byte: UInt8 = 0
            while read(conn, &byte, 1) == 1, byte != UInt8(ascii: "\n") { line.append(byte) }
            let reply = Array((answer(String(decoding: line, as: UTF8.self)) + "\n.\n").utf8)
            _ = reply.withUnsafeBytes { write(conn, $0.baseAddress, $0.count) }
        }
    }

    func stop() {
        close(fd)
        unlink(path)
    }
}
