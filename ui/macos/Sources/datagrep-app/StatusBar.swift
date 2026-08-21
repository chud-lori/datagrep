import DatagrepKit
import SwiftUI

struct StatusBar: View {
    @ObservedObject var model: AppModel

    private enum Density {
        case full, comfortable, compact, minimal
    }

    private var dotColor: Color {
        switch model.state {
        case .streaming, .parked: return .blue
        case .done: return .green
        case .capped, .cancelled: return .orange
        case .failed: return .red
        case nil: return .secondary
        }
    }

    private var limitHit: Int? {
        guard model.state == .done, let lim = model.directives.limit, lim > 0,
            model.rowsLoaded >= UInt64(lim)
        else { return nil }
        return lim
    }

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

    private var residentText: String {
        String(
            format: "resident %d pages / %@ rows · %.1f MB", model.residentPages,
            model.residentRows.formatted(), model.footprintMB)
    }

    private func message(at density: Density) -> String {
        let parts = model.message.components(separatedBy: " · ")
        guard parts.count > 1 else { return model.message }
        switch density {
        case .full, .comfortable:
            return model.message
        case .compact:
            return parts.dropLast().joined(separator: " · ")
        case .minimal:
            return parts[0]
        }
    }

    /// Everything the bar knows, including whatever this width had to hide.
    private var fullTooltip: String {
        var lines: [String] = [
            "state: \(model.state?.rawValue ?? "idle")",
            "rows: \(rowCountText) — \(rowCountHelp)",
            "elapsed: \(model.elapsedMs) ms",
            "\(residentText)  (phys_footprint, not ps RSS)",
        ]
        if !model.directives.summary.isEmpty {
            lines.append("directives: \(model.directives.summary)")
        }
        if model.hiddenColumns > 0 {
            lines.append(
                "\(model.hiddenColumns) columns hidden beyond the 30-column visible cap — new columns append on the right and never reorder"
            )
        }
        if let notice = incompleteNotice { lines.append(notice.text) }
        lines.append("")
        lines.append(model.message)
        return lines.joined(separator: "\n")
    }

    var body: some View {
        ViewThatFits(in: .horizontal) {
            bar(.full)
            bar(.comfortable)
            bar(.compact)
            bar(.minimal)
        }
        .font(.system(size: 11))
        .monospacedDigit()
        .padding(.horizontal, 12)
        .frame(height: 28)
        .background(.ultraThinMaterial)
        .overlay(alignment: .top) { Divider() }
        .animation(.smooth(duration: 0.2), value: model.state)
        .animation(.smooth(duration: 0.2), value: model.rowsLoaded)
    }

    @ViewBuilder
    private func bar(_ density: Density) -> some View {
        HStack(spacing: 12) {
            Circle()
                .fill(dotColor)
                .frame(width: 7, height: 7)
                .help(fullTooltip)

            Text(model.state?.rawValue ?? "idle")
                .monospaced()
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .fixedSize()
                .layoutPriority(3)

            Text(rowCountText)
                .monospaced()
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .fixedSize()
                .layoutPriority(3)
                .help(rowCountHelp)

            if let notice = incompleteNotice {
                Group {
                    if density == .minimal {
                        Image(systemName: notice.icon)
                    } else {
                        Label(notice.text, systemImage: notice.icon)
                    }
                }
                .foregroundStyle(.orange)
                .lineLimit(1)
                .fixedSize()
                .layoutPriority(2)
                .help(notice.help)
            }

            Text("\(model.elapsedMs) ms")
                .monospaced()
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .fixedSize()
                .layoutPriority(3)

            if density == .full {
                Text(residentText)
                    .monospaced()
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
                    .fixedSize()
                    .help("phys_footprint, not ps RSS")
            }

            if !model.directives.summary.isEmpty, density == .full || density == .comfortable {
                Text(model.directives.summary)
                    .monospaced()
                    .foregroundStyle(Color.accentColor)
                    .lineLimit(1)
                    .fixedSize()
                    .help("block directives applied to the statement under the caret")
            }

            if model.hiddenColumns > 0, density != .minimal {
                Group {
                    if density == .compact {
                        Label("\(model.hiddenColumns) hidden", systemImage: "rectangle.on.rectangle")
                    } else {
                        Label(
                            "\(model.hiddenColumns) columns hidden",
                            systemImage: "rectangle.on.rectangle")
                    }
                }
                .foregroundStyle(.orange)
                .lineLimit(1)
                .fixedSize()
                .help(
                    "beyond the 30-column visible cap; new columns append on the right and never reorder"
                )
            }

            Divider().frame(height: 12)

            Text(message(at: density))
                .foregroundStyle(model.isError ? Color.red : Color.primary)
                .lineLimit(1)
                .truncationMode(.tail)
                .frame(idealWidth: 220, alignment: .leading)
                .layoutPriority(1)
                .help(model.message)

            Spacer(minLength: 8)

            Button {
                model.cancel()
            } label: {
                Label("Cancel", systemImage: "stop.circle")
            }
            .disabled(!model.isRunning)
            .keyboardShortcut(".", modifiers: .command)
            .fixedSize()
            .help("Cancel the running statement  ⌘.")
        }
    }
}
