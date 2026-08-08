import DatagrepKit
import SwiftUI

/// Rows loaded, elapsed ms, connection state, resident-window accounting, and
/// Cancel. Nothing here animates and nothing polls — every value changes only on
/// a progress callback or a user action. (No `ProgressView()` spinner: an
/// indeterminate spinner animates forever and would fail P19 on its own.)
///
/// ## One line, at every width
///
/// A status bar that wraps is worse than one that hides something: at 1180 pt
/// this bar used to spill `resident 0 pages / 0 rows · 11.7 MB` onto a second
/// line that collided with the grid above it, and truncate the message
/// mid-word into `core ready · 4 profil…othing connected yet` — a string that
/// reads like two different facts spliced together, which is exactly the kind
/// of "silently wrong" the rest of this app goes out of its way to avoid.
///
/// So the bar is built at four densities and `ViewThatFits` picks the widest
/// one that actually fits, in this order:
///
///   `.full`         everything
///   `.comfortable`  drops `resident … MB` (a diagnostic, not a result)
///   `.compact`      drops the block directives and shortens the message
///   `.minimal`      state, rows, elapsed, message, Cancel — and nothing else
///
/// The invariants that make that safe:
///
///   * every field is `.lineLimit(1)`, so nothing can ever wrap;
///   * the fields that must survive (state, row count, elapsed) are
///     `.fixedSize()`, so the layout drops an optional field rather than
///     squeezing a required one;
///   * the message is the only elastic field and tail-truncates — never
///     `.middle`, which is what manufactured the gibberish above;
///   * **nothing is lost**: the status dot's tooltip carries every field,
///     including the ones the current width dropped, in full.
struct StatusBar: View {
    @ObservedObject var model: AppModel

    /// How much of the bar there is room for. Ordered widest-first, which is
    /// also the order `ViewThatFits` tries them in.
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

    private var residentText: String {
        String(
            format: "resident %d pages / %@ rows · %.1f MB", model.residentPages,
            model.residentRows.formatted(), model.footprintMB)
    }

    /// Messages in this app are clauses joined with " · " — `boot()` writes
    /// "core ready · 4 profiles · nothing connected yet", `reportFootprint()`
    /// writes eight of them. At widths where the whole line will not fit, drop
    /// whole trailing clauses rather than letting the renderer cut a word in
    /// half: a shorter true sentence beats a longer broken one, and the
    /// tooltip has the rest either way.
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
    /// Hung off the status dot, which is present at every density.
    private var fullTooltip: String {
        var lines: [String] = [
            "state: \(model.state?.rawValue ?? "idle")",
            "rows: \(rowCountText) — \(rowCountHelp)",
            "elapsed: \(model.elapsedMs) ms",
            "\(residentText)  (phys_footprint per design §5 measurement semantics, not ps RSS)",
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
        // Widest-first. If none of them fit — a window narrower than the 900 pt
        // minimum should not be reachable, but a huge inspector plus a long
        // error message can get close — `ViewThatFits` uses the last, and the
        // message truncates inside it. There is no arrangement in which this
        // becomes two lines.
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
        // A material, not a fill: the status bar floats above the content
        // instead of merging into the same flat plane as everything else.
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

            // The three that never drop. `.fixedSize()` is what guarantees it:
            // without it the layout would shrink these to fit an optional field
            // it should have dropped instead.
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
                // A partial result is never silently dropped for want of room —
                // at the narrowest density it collapses to its icon, which
                // still carries the full explanation as a tooltip.
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

            // Diagnostics. First to go, because they are the only fields whose
            // absence costs the user nothing they cannot get from the tooltip.
            if density == .full {
                Text(residentText)
                    .monospaced()
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
                    .fixedSize()
                    .help("phys_footprint (design §5 measurement semantics), not ps RSS")
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

            // The one elastic field. Tail truncation, never `.middle`: the
            // middle form is what produced "4 profil…othing connected yet",
            // where the ellipsis hides the seam between two clauses and the
            // result reads as one nonsensical sentence. A tail ellipsis is
            // unambiguously "there is more, and the tooltip has it".
            Text(message(at: density))
                .foregroundStyle(model.isError ? Color.red : Color.primary)
                .lineLimit(1)
                .truncationMode(.tail)
                // `idealWidth` is what keeps a long message from bullying the
                // rest of the bar. `ViewThatFits` sizes each candidate by
                // proposing it nothing and reading back its ideal width, and an
                // unconstrained `Text` answers with the width of the whole
                // string — so one `reportFootprint()` line (eight clauses) would
                // force `.minimal` at 1180 pt and hide an incomplete-result
                // warning that had 400 pt of room to sit in. Capping the ideal
                // says "assume this field takes 220 pt and truncates", which is
                // what it in fact does; it still expands into whatever slack is
                // left over.
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
