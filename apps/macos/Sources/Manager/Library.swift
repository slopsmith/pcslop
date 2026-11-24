import Foundation
import ManagerCore
import Observation

@MainActor
@Observable
final class Library {
    let store: MachineStore
    private(set) var machines: [Machine] = []
    private(set) var states: [Machine.ID: [SavedState]] = [:]
    private(set) var thumbnailVersion = 0
    var lastError: String?

    init(store: MachineStore) {
        self.store = store
        machines = store.loadAll()
        for machine in machines { states[machine.id] = store.states(of: machine.id) }
    }

    func machine(_ id: Machine.ID?) -> Machine? {
        machines.first { $0.id == id }
    }

    func save(_ machine: Machine) {
        do {
            try store.save(machine)
        } catch {
            lastError = "Cannot save \(machine.name): \(error.localizedDescription)"
            return
        }
        if let i = machines.firstIndex(where: { $0.id == machine.id }) {
            machines[i] = machine
        } else {
            machines.append(machine)
        }
        machines.sort { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
    }

    func duplicate(_ machine: Machine) -> Machine {
        var copy = machine
        copy.id = UUID()
        copy.name = "\(machine.name) copy"
        copy.created = .now
        save(copy)
        return copy
    }

    func delete(_ id: Machine.ID) {
        do {
            try store.delete(id)
        } catch {
            lastError = "Cannot delete the machine: \(error.localizedDescription)"
            return
        }
        machines.removeAll { $0.id == id }
        states[id] = nil
    }

    func refreshStates(of id: Machine.ID) {
        states[id] = store.states(of: id)
    }

    func delete(_ state: SavedState, of id: Machine.ID) {
        do {
            try store.deleteState(state)
        } catch {
            lastError = "Cannot delete \(state.name): \(error.localizedDescription)"
        }
        refreshStates(of: id)
    }

    // pcslop writes screenshots on a thread of its own after answering `shot`.
    func thumbnailsChanged() {
        Task {
            try? await Task.sleep(for: .seconds(1))
            thumbnailVersion += 1
        }
    }
}
