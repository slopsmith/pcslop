import Foundation
import Testing

@testable import ManagerCore

struct PresetTests {
    private let allFilesExist: (String) -> Bool = { _ in true }

    @Test(arguments: [Preset.windowsXPRadeon, .ubuntuRadeon])
    func radeonPresetsUseTheRadeonWhenThereIsAVideoBIOS(preset: Preset) {
        let machine = preset.machine(name: "M", disk: "/d.img", cdrom: "/c.iso", radeon: true)
        #expect(machine.config.gpu == .radeon)
        #expect(machine.config.problems(radeonROM: "/r.rom", fileExists: allFilesExist).isEmpty)
    }

    @Test(arguments: [Preset.windowsXPRadeon, .ubuntuRadeon])
    func radeonPresetsFallBackToStandardVGA(preset: Preset) {
        let machine = preset.machine(name: "M", disk: "/d.img", cdrom: nil, radeon: false)
        #expect(machine.config.gpu == .stdvga)
        #expect(machine.config.problems(radeonROM: nil, fileExists: allFilesExist).isEmpty)
    }

    @Test(arguments: [true, false])
    func theCustomPresetKeepsTheDefaultHardware(radeon: Bool) {
        let machine = Preset.custom.machine(name: "M", disk: nil, cdrom: nil, radeon: radeon)
        #expect(machine.config == MachineConfig())
    }

    @Test(arguments: Preset.allCases)
    func everyPresetPutsTheDisksWhereTheyWereGiven(preset: Preset) {
        let machine = preset.machine(name: "M", disk: "/d.img", cdrom: "/c.iso", radeon: true)
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
