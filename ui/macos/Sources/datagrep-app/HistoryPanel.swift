import AppKit
import DatagrepKit
import SwiftUI

/// Query history: everything datagrep has actually run, searchable.
///
/// The shape is taken from what the UX study says the competitors get *wrong*,
/// not from what they look like:
///
/// * **Searchable by text, connection and date.** DBeaver's history cannot be
///   searched by date and has no retention control ([#22238]); both are here, in
///   the filter bar, one click deep.
/// * **Retention is stated and editable, never a silent cap.** Sequel Ace stops
///   at 100 entries without saying so ([#1551]). The footer says exactly what is
///   being kept and lets you change it.
/// * **Not scoped to whatever you are connected to.** HeidiSQL's users asked for
///   precisely this ([#1142]) — connection is a filter you may apply, not one
///   applied for you.
/// * **Failures are kept, with their error.** The query you want back is usually
///   the one that broke.
/// * **Separate from Saved Queries.** An automatic log and a curated list are two
///   different things; `SavedQueries.swift` owns the curated half and this file
///   never touches it.
///
/// Every colour here is semantic (`NSColor`/`ShapeStyle` roles), so light and
/// dark are the same code path — four separate competitors have shipped literally
/// unreadable dark mode, and none of them meant to.
struct HistoryPanel: View {
    @ObservedObject var model: HistoryModel
    /// Present as a sheet? Then this is the dismissal. Nil when hosted inline.
    var onClose: (() -> Void)? = nil

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            filterBar
            Divider()
            content
            if let entry = model.selected {
                Divider()
                HistoryDetail(entry: entry, model: model)
            }
            Divider()
            footer
        }
        .frame(minWidth: 620, idealWidth: 760, minHeight: 460, idealHeight: 620)
        .background(Color(nsColor: .windowBackgroundColor))
        .animation(.smooth(duration: 0.18), value: model.selectedID)
    }

    // MARK: header

    private var header: some View {
        HStack(spacing: 8) {
            Image(systemName: "clock.arrow.circlepath")
                .foregroundStyle(Color.accentColor)
            VStack(alignment: .leading, spacing: 1) {
                Text("Query History").font(.headline)
                Text(subtitle)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 12)
            if let onClose {
                Button { onClose() } label: { Image(systemName: "xmark") }
                    .buttonStyle(.borderless)
                    .foregroundStyle(.secondary)
                    .keyboardShortcut(.cancelAction)
                    .help("Close (⎋)")
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    private var subtitle: String {
        let total = model.entries.count
        guard total > 0 else { return "nothing has been run yet" }
        let shown = model.filtered.count
        let word = total == 1 ? "query" : "queries"
        return model.hasFilter
            ? "\(shown.formatted()) of \(total.formatted()) \(word)"
            : "\(total.formatted()) \(word)"
    }

    // MARK: filters

    private var filterBar: some View {
        HStack(spacing: 8) {
            HStack(spacing: 5) {
                Image(systemName: "magnifyingglass")
                    .foregroundStyle(.secondary)
                    .font(.system(size: 11))
                TextField("Search SQL and error text", text: $model.search)
                    .textFieldStyle(.plain)
                    .font(.system(size: 12))
                if !model.search.isEmpty {
                    Button { model.search = "" } label: {
                        Image(systemName: "xmark.circle.fill")
                    }
                    .buttonStyle(.borderless)
                    .foregroundStyle(.secondary)
                }
            }
            .padding(.horizontal, 7)
            .padding(.vertical, 4)
            .background(
                RoundedRectangle(cornerRadius: 6, style: .continuous)
                    .fill(Color(nsColor: .textBackgroundColor))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 6, style: .continuous)
                    .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 1)
            )
            .frame(minWidth: 180)

            // Connection: "All" first and default. History is never scoped for
            // you — see [#1142] in the file header.
            Picker("", selection: $model.connectionFilter) {
                Text("All connections").tag(String?.none)
                if !model.knownConnections.isEmpty { Divider() }
                ForEach(model.knownConnections, id: \.self) { name in
                    Text(name).tag(String?.some(name))
                }
            }
            .labelsHidden()
            .frame(maxWidth: 190)
            .help("Filter by the connection a statement was run against")

            Picker("", selection: $model.range) {
                ForEach(HistoryDateRange.allCases) { r in Text(r.title).tag(r) }
            }
            .labelsHidden()
            .pickerStyle(.segmented)
            .fixedSize()
            .help("Filter by when the statement ran")

            Menu {
                Button { model.outcomeFilter = nil } label: {
                    Label("Any outcome", systemImage: model.outcomeFilter == nil ? "checkmark" : "")
                }
                ForEach(QueryOutcome.allCases, id: \.self) { o in
                    Button { model.outcomeFilter = o } label: {
                        Label(
                            o.label.capitalized,
                            systemImage: model.outcomeFilter == o ? "checkmark" : o.symbol)
                    }
                }
            } label: {
                Image(systemName: outcomeGlyph)
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
            .help("Filter by outcome — ok, failed or cancelled")

            if model.hasFilter {
                Button("Clear") { model.clearFilters() }
                    .controlSize(.small)
                    .help("Remove every filter")
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    private var outcomeGlyph: String {
        model.outcomeFilter?.symbol ?? "line.3.horizontal.decrease.circle"
    }

    // MARK: list

    @ViewBuilder private var content: some View {
        if model.entries.isEmpty {
            ContentUnavailableView {
                Label("No history yet", systemImage: "clock.arrow.circlepath")
            } description: {
                Text(
                    "Every statement datagrep runs is logged here automatically — the SQL, which connection it ran on, how long it took, and what came back."
                )
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if model.filtered.isEmpty {
            ContentUnavailableView {
                Label("No matches", systemImage: "magnifyingglass")
            } description: {
                Text("No recorded query matches these filters.")
            } actions: {
                Button("Clear filters") { model.clearFilters() }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            List(selection: $model.selectedID) {
                ForEach(model.days) { day in
                    Section {
                        ForEach(day.entries) { entry in
                            HistoryRow(entry: entry)
                                .tag(entry.id)
                                .contextMenu { rowMenu(entry) }
                        }
                    } header: {
                        Text(day.title)
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.secondary)
                    }
                }
            }
            .listStyle(.inset)
            .frame(maxHeight: .infinity)
        }
    }

    @ViewBuilder private func rowMenu(_ entry: QueryHistoryEntry) -> some View {
        Button("Copy SQL") { model.copy(entry) }
        Button("Open in New Editor Tab") { model.openInEditor(entry) }
        Button("Run Again") { model.rerun(entry) }
        Divider()
        Button("Remove from History", role: .destructive) { model.delete(entry) }
    }

    // MARK: footer

    private var footer: some View {
        HStack(spacing: 10) {
            Image(systemName: "externaldrive")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
            // Stated, not hidden: this is the whole point of the retention work.
            Text(model.retention.summary)
                .font(.caption)
                .foregroundStyle(.secondary)
            Spacer(minLength: 8)
            RetentionButton(model: model)
            Menu {
                if let c = model.connectionFilter {
                    Button("Clear History for “\(c)”", role: .destructive) {
                        model.clearCurrentConnection()
                    }
                }
                Button("Clear All History", role: .destructive) { model.clearAll() }
            } label: {
                Text("Clear…")
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
            .disabled(model.entries.isEmpty)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
    }
}

// MARK: - one row

/// One recorded statement, one or two lines of it.
///
/// Monospaced, whitespace collapsed, truncated at the tail — never reformatted
/// and never re-indented. The full text is one click away in the detail strip
/// below, so this line is allowed to be short.
private struct HistoryRow: View {
    let entry: QueryHistoryEntry

    var body: some View {
        HStack(alignment: .top, spacing: 9) {
            Image(systemName: entry.outcome.symbol)
                .foregroundStyle(HistoryStyle.tint(entry.outcome))
                .font(.system(size: 11))
                .frame(width: 14)
                .padding(.top, 2)
                .accessibilityLabel(entry.outcome.label)

            VStack(alignment: .leading, spacing: 3) {
                Text(entry.oneLine)
                    .font(.system(size: 11.5, design: .monospaced))
                    .lineLimit(2)
                    .truncationMode(.tail)
                    .textSelection(.disabled)

                HStack(spacing: 6) {
                    if !entry.engine.isEmpty { EngineIcon(entry.engine, size: 11) }
                    Text(entry.connection.isEmpty ? "no connection" : entry.connection)
                    Text("·")
                    Text(HistoryFormat.time(entry.startedAt))
                    Text("·")
                    Text(HistoryFormat.duration(entry.durationMs))
                    if let rows = HistoryFormat.rows(entry.rowCount), entry.outcome == .ok {
                        Text("·")
                        Text(rows)
                    }
                }
                .font(.system(size: 10))
                .foregroundStyle(.secondary)
                .lineLimit(1)
            }

            Spacer(minLength: 4)

            if entry.runCount > 1 {
                Text("×\(entry.runCount)")
                    .font(.system(size: 9.5, weight: .medium))
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1.5)
                    .background(
                        Capsule().fill(Color(nsColor: .quaternaryLabelColor).opacity(0.45))
                    )
                    .help("Run \(entry.runCount) times in quick succession — collapsed into one entry")
                    .padding(.top, 2)
            }
        }
        .padding(.vertical, 3)
    }
}

// MARK: - selected entry

/// The full statement, its error if it had one, and the three things you would
/// want to do with it.
private struct HistoryDetail: View {
    let entry: QueryHistoryEntry
    @ObservedObject var model: HistoryModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                Image(systemName: entry.outcome.symbol)
                    .foregroundStyle(HistoryStyle.tint(entry.outcome))
                    .font(.system(size: 11))
                Text(summary)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                Spacer(minLength: 8)
                Button { model.copy(entry) } label: { Label("Copy", systemImage: "doc.on.doc") }
                    .controlSize(.small)
                    .help("Copy the statement to the clipboard")
                Button { model.openInEditor(entry) } label: {
                    Label("Open in Editor", systemImage: "square.and.pencil")
                }
                .controlSize(.small)
                .help("Open this statement in a new editor tab, bound to the connection it ran on")
                Button { model.rerun(entry) } label: { Label("Run Again", systemImage: "play.fill") }
                    .controlSize(.small)
                    .buttonStyle(.borderedProminent)
                    .help("Run this statement again now")
            }

            ScrollView {
                Text(entry.sql)
                    .font(.system(size: 11.5, design: .monospaced))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(8)
            }
            .frame(height: entry.error == nil ? 92 : 64)
            .background(
                RoundedRectangle(cornerRadius: 6, style: .continuous)
                    .fill(Color(nsColor: .textBackgroundColor))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 6, style: .continuous)
                    .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 1)
            )

            if let error = entry.error, !error.isEmpty {
                ScrollView {
                    Text(error)
                        .font(.system(size: 11, design: .monospaced))
                        .foregroundStyle(HistoryStyle.tint(.error))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(7)
                }
                .frame(height: 56)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(HistoryStyle.tint(.error).opacity(0.09))
                )
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
    }

    private var summary: String {
        var parts: [String] = []
        if !entry.connection.isEmpty {
            parts.append(
                entry.engine.isEmpty
                    ? entry.connection
                    : "\(entry.connection) · \(EngineStyle.displayName(for: entry.engine))")
        }
        parts.append(HistoryFormat.dayTitle(for: entry.startedAt) + " " + HistoryFormat.time(entry.startedAt))
        parts.append(HistoryFormat.duration(entry.durationMs))
        if entry.outcome == .ok, let rows = HistoryFormat.rows(entry.rowCount) {
            parts.append(rows)
        } else if entry.outcome != .ok {
            parts.append(entry.outcome.label)
        }
        if entry.runCount > 1 { parts.append("run \(entry.runCount)×") }
        return parts.joined(separator: "  ·  ")
    }
}

// MARK: - retention

/// The fix for the two documented failures in one small popover: DBeaver has no
/// retention control at all, Sequel Ace has one nobody can see or change.
private struct RetentionButton: View {
    @ObservedObject var model: HistoryModel
    @State private var showing = false
    @State private var maxEntries: String = ""
    @State private var maxDays: String = ""

    var body: some View {
        Button("Retention…") {
            maxEntries = "\(model.retention.maxEntries)"
            maxDays = "\(model.retention.maxDays)"
            showing = true
        }
        .controlSize(.small)
        .popover(isPresented: $showing, arrowEdge: .bottom) {
            VStack(alignment: .leading, spacing: 12) {
                Text("How much history to keep").font(.headline)
                Text(
                    "datagrep keeps whichever limit is reached first. Entries are stored as one plain JSON-lines file per day in ~/Library/Application Support/datagrep/history/, so nothing here is locked away."
                )
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(width: 320, alignment: .leading)

                Grid(alignment: .leading, horizontalSpacing: 10, verticalSpacing: 8) {
                    GridRow {
                        Text("Entries").foregroundStyle(.secondary)
                        TextField("10000", text: $maxEntries)
                            .textFieldStyle(.roundedBorder)
                            .frame(width: 90)
                        Text("newest kept").font(.caption).foregroundStyle(.secondary)
                    }
                    GridRow {
                        Text("Days").foregroundStyle(.secondary)
                        TextField("180", text: $maxDays)
                            .textFieldStyle(.roundedBorder)
                            .frame(width: 90)
                        Text("older entries dropped").font(.caption).foregroundStyle(.secondary)
                    }
                }

                HStack {
                    Button("Reset to defaults") {
                        maxEntries = "\(HistoryRetention.default.maxEntries)"
                        maxDays = "\(HistoryRetention.default.maxDays)"
                    }
                    .controlSize(.small)
                    Spacer()
                    Button("Apply") {
                        model.setRetention(
                            HistoryRetention(
                                maxEntries: Int(maxEntries) ?? model.retention.maxEntries,
                                maxDays: Int(maxDays) ?? model.retention.maxDays))
                        showing = false
                    }
                    .keyboardShortcut(.defaultAction)
                    .controlSize(.small)
                }
            }
            .padding(14)
        }
        .help("Choose how many queries, and how many days, of history to keep")
    }
}

// MARK: - hosting

/// Presents the panel as a sheet, driven by `HistoryModel.isPresented`.
///
/// A modifier and not a raw `.sheet` in `Workbench`: `HistoryModel` is a nested
/// `ObservableObject`, so whoever owns the `isPresented` binding has to be
/// observing *it* — a binding reached through `AppModel` would set the flag and
/// then never redraw. This owns that observation, so the host adds one line.
///
///     .historySheet(model.history)
struct HistorySheet: ViewModifier {
    @ObservedObject var history: HistoryModel

    func body(content: Content) -> some View {
        content.sheet(isPresented: $history.isPresented) {
            HistoryPanel(model: history) { history.isPresented = false }
        }
    }
}

extension View {
    func historySheet(_ history: HistoryModel) -> some View {
        modifier(HistorySheet(history: history))
    }
}

/// The toolbar control that opens it. Same reason as above — it observes the
/// history model so the badge-free count in its tooltip stays true.
struct HistoryToolbarButton: View {
    @ObservedObject var history: HistoryModel

    var body: some View {
        Button {
            history.isPresented = true
        } label: {
            Label("History", systemImage: "clock.arrow.circlepath")
        }
        .help(
            history.entries.isEmpty
                ? "Query history — every statement datagrep runs is logged here  ⌘Y"
                : "Query history — \(history.entries.count.formatted()) statements, searchable  ⌘Y"
        )
    }
}

// MARK: - colour

/// One definition of what an outcome looks like. All three are system semantic
/// colours, so they are legible in both appearances without a second palette —
/// unreadable dark mode comes from hardcoded colour values, every time.
enum HistoryStyle {
    static func tint(_ outcome: QueryOutcome) -> Color {
        switch outcome {
        case .ok: return Color(nsColor: .systemGreen)
        case .error: return Color(nsColor: .systemRed)
        case .cancelled: return Color(nsColor: .systemOrange)
        }
    }
}
