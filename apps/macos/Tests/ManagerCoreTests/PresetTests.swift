import Foundation
import Testing

@testable import ManagerCore

struct PresetTests {
    private let allFilesExist: (String) -> Bool = { _ in true }

    @Test(arguments: [Preset.windowsXPRadeon, .ubuntuRadeon])
    func radeonPresetsMakeAMachinePcslopAccepts(preset: Preset) {
        let machine = preset.machine(name: "M", disk: "/d.img", cdrom: "/c.iso", vgaROM: "/r.rom")
        #expect(machine.config.problems(fileExists: allFilesExist).isEmpty)
        #expect(machine.config.gpu == .radeon)
    }

    @Test(arguments: [Preset.windowsXPRadeon, .ubuntuRadeon])
    func radeonPresetsNeedTheVideoBIOS(preset: Preset) {
        let machine = preset.machine(name: "M", disk: "/d.img", cdrom: nil, vgaROM: nil)
        #expect(!machine.config.problems(fileExists: allFilesExist).isEmpty)
        #expect(preset.needsVideoBIOS)
    }

    @Test func theCustomPresetKeepsTheDefaultHardware() {
        let machine = Preset.custom.machine(name: "M", disk: nil, cdrom: nil, vgaROM: "/r.rom")
        var expected = MachineConfig()
        expected.vgaROM = nil
        #expect(machine.config == expected)
        #expect(!Preset.custom.needsVideoBIOS)
    }

    @Test(arguments: Preset.allCases)
    func everyPresetPutsTheDisksWhereTheyWereGiven(preset: Preset) {
        let machine = preset.machine(name: "M", disk: "/d.img", cdrom: "/c.iso", vgaROM: "/r.rom")
        #expect(machine.config.hda == "/d.img")
        #expect(machine.config.cdrom == "/c.iso")
        #expect(machine.os == preset.os)
    }

    @Test(arguments: Preset.allCases)
    func everyPresetOffersADiskThatFitsItsSystem(preset: Preset) {
        #expect(DiskSize.range.contains(preset.diskGiB))
    }
}

struct MemorySizeTests {
    @Test func stepsGrowAndStayInsideTheRange() {
        #expect(MemorySize.steps == MemorySize.steps.sorted())
        #expect(MemorySize.steps.first == MemorySize.range.lowerBound)
        #expect(MemorySize.steps.last == MemorySize.range.upperBound)
    }

    @Test func aStepSizePicksItsOwnStep() {
        for (i, mib) in MemorySize.steps.enumerated() {
            #expect(MemorySize.nearestStep(to: mib) == i)
        }
    }

    @Test func anOddSizePicksTheClosestStep() {
        let i = MemorySize.nearestStep(to: 500)
        #expect(MemorySize.steps[i] == 512)
    }

    @Test func sizesOutsideTheRangePickTheEnds() {
        #expect(MemorySize.nearestStep(to: 0) == 0)
        #expect(MemorySize.nearestStep(to: 1 << 20) == MemorySize.steps.count - 1)
    }

    @Test func clampingKeepsTypedSizesInsideTheRange() {
        #expect(MemorySize.clamped(1) == MemorySize.range.lowerBound)
        #expect(MemorySize.clamped(700) == 700)
        #expect(MemorySize.clamped(99_999) == MemorySize.range.upperBound)
    }
}

struct RadeonROMLocatorTests {
    @Test func findsAROMInTheReferenceDocs() throws {
        let repo = FileManager.default.temporaryDirectory.appendingPathComponent("pcslop-repo-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: repo) }
        let folder = repo.appendingPathComponent("references/docs/ati-r300")
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        try Data().write(to: folder.appendingPathComponent("notes.txt"))
        let rom = folder.appendingPathComponent("card.rom")
        try Data().write(to: rom)
        let deep = repo.appendingPathComponent("apps/macos")
        try FileManager.default.createDirectory(at: deep, withIntermediateDirectories: true)
        #expect(BinaryLocator.radeonROM(searchFrom: [deep])?.standardizedFileURL.path == rom.standardizedFileURL.path)
    }

    @Test func findsNothingWithoutTheReferenceDocs() throws {
        let empty = FileManager.default.temporaryDirectory.appendingPathComponent("pcslop-none-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: empty, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: empty) }
        #expect(BinaryLocator.radeonROM(searchFrom: [empty]) == nil)
    }
}
