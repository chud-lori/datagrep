import SwiftUI

/// Keeps the editor and grid bridges off the startup critical path.
@MainActor
final class StartupStage: ObservableObject {
    static let shared = StartupStage()

    /// False until the window has been painted at least once.
    @Published private(set) var contentReady: Bool

    private init() {
        contentReady = ProcessInfo.processInfo.arguments.contains("--no-deferred-content")
    }

    func markContentReady() {
        guard !contentReady else { return }
        contentReady = true
    }
}
