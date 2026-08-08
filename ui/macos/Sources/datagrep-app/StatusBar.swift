import DatagrepKit
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

    /// The `@limit N` directive stopped the result at exactly N rows — the
    /// grid holds the first N of a possibly longer result (this is what every
    /// "Preview 500 Rows" produces, via its injected `-- @limit 500`). If the
    /// result came back UNDER the limit, the limit never bit and there is
    /// nothing to announce.
    private var limitHit: Int? {
        guard model.state == .done, let lim = model.directives.limit, lim > 0,
            model.rowsLoaded >= UInt64(lim)
        else { return nil }
        return lim
    }

    /// A partial result must not print a row count that looks final (the
    /// RedisInsight lesson: silently incomplete views read as "the data does
    /// not exist"). Three honest forms:
    ///   capped / limit-hit  ->  "first N rows"
    ///   total unknown       ->  "≥ N rows"
    ///   complete            ->  "N rows"
    private var rowCountText: String {
        let n = model.rowsLoaded.formatted()
        if model.state == .capped || limitHit != nil { return "first \(n) rows" }
        return model.totalKnown ? "\(n) rows" : "≥ \(n) rows"
    }

    private var rowCountHelp: String {
        if model.state == .capped {
            return "the engine stopped storing rows at its cap — this is not the whole result"
        }
        if limitHit != nil {
            return "the statement ran with an @limit directive — this is not the whole result"
        }
        if !model.totalKnown {
            return
                "≥ because this engine streams without reporting a total — more rows may exist than have been loaded"
        }
        return "row count reported by the engine"
    }

    /// The unmissable version, in words, next to the count: plain, specific
    /// phrasing rather than a vague badge.
    private var incompleteNotice: (text: String, icon: String, help: String)? {
        if model.state == .capped {
            return (
                "stopped at \(model.rowsLoaded.formatted())-row limit — result incomplete",
                "exclamationmark.triangle.fill",
                "the engine's soft row cap ended this result early; rows beyond this point exist but were not fetched — narrow the query to see them"
            )
        }
        if let lim = limitHit {
            return (
                "showing first \(lim.formatted()) rows of more (@limit)",
                "arrow.down.to.line",
                "an @limit \(lim) directive stopped this result at \(lim.formatted()) rows — the full result may be longer; raise or remove the @limit to fetch more"
            )
        }
        return nil
    }

    var body: some View {
        HStack(spacing: 12) {
            Circle().fill(dotColor).frame(width: 7, height: 7)

            Text(model.state?.rawValue ?? "idle")
                .monospaced()
                .foregroundStyle(.secondary)

            Text(rowCountText)
                .monospaced()
                .foregroundStyle(.secondary)
                .help(rowCountHelp)

            if let notice = incompleteNotice {
                Label(notice.text, systemImage: notice.icon)
                    .foregroundStyle(.orange)
                    .lineLimit(1)
                    .help(notice.help)
            }

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
