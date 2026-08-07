import DbxKit
import SwiftUI

/// Rows loaded, elapsed ms, connection state, resident-window accounting, and
/// Cancel. Nothing here animates and nothing polls — every value changes only on
/// a progress callback or a user action. (No `ProgressView()` spinner: an
/// indeterminate spinner animates forever and would fail P19 on its own.)
struct StatusBar: View {
    @ObservedObject var model: AppModel

    private var dotColor: Color {
        switch model.state {
        case .streaming, .parked: return .blue
        case .done: return .green
        case .capped, .cancelled: return .orange
        case .failed: return .red
        case nil: return .secondary
        }
    }

    var body: some View {
        HStack(spacing: 12) {
            Circle().fill(dotColor).frame(width: 7, height: 7)

            Text(model.state?.rawValue ?? "idle")
                .monospaced()
                .foregroundStyle(.secondary)

            Text("\(model.totalKnown ? "" : "≥ ")\(model.rowsLoaded.formatted()) rows")
                .monospaced()
                .foregroundStyle(.secondary)

            Text("\(model.elapsedMs) ms")
                .monospaced()
                .foregroundStyle(.secondary)

            Text(
                String(
                    format: "resident %d pages / %@ rows · %.1f MB", model.residentPages,
                    model.residentRows.formatted(), model.footprintMB)
            )
            .monospaced()
            .foregroundStyle(.tertiary)
            .help("phys_footprint (design §5 measurement semantics), not ps RSS")

            if !model.directives.summary.isEmpty {
                Text(model.directives.summary)
                    .monospaced()
                    .foregroundStyle(Color.accentColor)
                    .help("block directives applied to the statement under the caret")
            }

            if model.hiddenColumns > 0 {
                Label("\(model.hiddenColumns) columns hidden", systemImage: "rectangle.on.rectangle")
                    .foregroundStyle(.orange)
                    .help(
                        "beyond the 30-column visible cap; new columns append on the right and never reorder"
                    )
            }

            Divider().frame(height: 12)

            Text(model.message)
                .foregroundStyle(model.isError ? Color.red : Color.primary)
                .lineLimit(1)
                .truncationMode(.middle)
                .help(model.message)

            Spacer(minLength: 8)

            Button {
                model.cancel()
            } label: {
                Label("Cancel", systemImage: "stop.circle")
            }
            .disabled(!model.isRunning)
            .keyboardShortcut(".", modifiers: .command)
        }
        .font(.system(size: 11))
        .monospacedDigit()
        .padding(.horizontal, 12)
        .frame(height: 28)
        // A material, not a fill: the status bar floats above the content
        // instead of merging into the same flat plane as everything else.
        .background(.ultraThinMaterial)
        .overlay(alignment: .top) { Divider() }
        .animation(.smooth(duration: 0.2), value: model.state)
        .animation(.smooth(duration: 0.2), value: model.rowsLoaded)
    }
}
