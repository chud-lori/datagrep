import AppKit
import DatagrepKit
import SwiftUI

struct SchemaPane: View {
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            content
        }
    }

    // MARK: header

    private var header: some View {
        HStack(spacing: 8) {
            Image(systemName: NodeStyle.symbol(forKind: model.schemaTarget?.kind ?? "table"))
                .foregroundStyle(Color.accentColor)
                .frame(width: 16)

            VStack(alignment: .leading, spacing: 1) {
                Text(model.schemaTarget?.name ?? "Schema")
                    .font(.headline)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text(subtitle)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.head)
            }

            Spacer(minLength: 4)

            if case .loaded(let detail) = model.schemaLoad {
                CopyMenu(detail: detail, model: model)
            }

            Button {
                model.refreshSchema()
            } label: {
                Image(systemName: "arrow.clockwise")
            }
            .buttonStyle(.borderless)
            .disabled(model.schemaTarget == nil || isLoading)
            .help("Re-read this object's schema from the server")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
    }

    private var subtitle: String {
        guard let t = model.schemaTarget else { return "nothing selected" }
        let where_ = t.path.dropLast().joined(separator: " › ")
        return where_.isEmpty ? "\(t.profile) · \(t.kind)" : "\(t.profile) · \(where_)"
    }

    private var isLoading: Bool {
        if case .loading = model.schemaLoad { return true }
        return false
    }

    // MARK: body

    @ViewBuilder private var content: some View {
        switch model.schemaLoad {
        case .idle:
            SchemaMessage(
                symbol: "tablecells",
                text: "Select a table, view, collection or key in the sidebar to see its structure."
            )
        case .loading:
            VStack(spacing: 9) {
                Spacer()
                ProgressView().controlSize(.small)
                Text("reading the schema…")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .failed(let message):
            VStack(spacing: 9) {
                Spacer()
                Image(systemName: "exclamationmark.triangle")
                    .font(.system(size: 22))
                    .foregroundStyle(.orange)
                Text(message)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .textSelection(.enabled)
                    .padding(.horizontal, 18)
                Button("Try again") { model.refreshSchema() }
                    .controlSize(.small)
                Spacer()
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .loaded(let detail):
            ScrollView(.vertical) {
                VStack(alignment: .leading, spacing: 14) {
                    StatsStrip(detail: detail)
                    if let c = detail.comment, !c.isEmpty {
                        Text(c)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                            .fixedSize(horizontal: false, vertical: true)
                            .padding(.horizontal, 12)
                    }
                    ColumnsSection(detail: detail, model: model)
                    IndexesSection(detail: detail)
                    ExtraSection(detail: detail)
                }
                .padding(.vertical, 12)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }
}

// MARK: - stats

private struct StatsStrip: View {
    let detail: SchemaDetail

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            if !facts.isEmpty {
                Text(facts.joined(separator: "  ·  "))
                    .font(.system(size: 10.5))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if detail.inferred {
                Label {
                    Text(inferredSentence)
                        .fixedSize(horizontal: false, vertical: true)
                } icon: {
                    Image(systemName: "wand.and.stars")
                }
                .font(.system(size: 10))
                .foregroundStyle(.secondary)
                .padding(.horizontal, 7)
                .padding(.vertical, 5)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(Color(nsColor: .quaternaryLabelColor).opacity(0.35)))
            }
        }
        .padding(.horizontal, 12)
    }

    private var facts: [String] {
        var out: [String] = []
        if let n = detail.rowEstimate {
            let unit = detail.kind == "collection" ? "documents" : "rows"
            out.append("≈ \(SchemaDetail.formatCount(n)) \(unit)")
        }
        if let c = detail.columnCount {
            out.append("\(c) \(detail.inferred ? "fields" : "columns")")
        }
        if let label = detail.sizeLabel {
            out.append(label)
        } else if let b = detail.sizeBytes {
            out.append(SchemaDetail.formatBytes(b))
        }
        return out
    }

    private var inferredSentence: String {
        if let n = detail.sampledDocs {
            return
                "Fields inferred from \(SchemaDetail.formatCount(n)) sampled documents — this engine declares no schema, so a field missing here may still exist elsewhere in the collection."
        }
        return
            "Fields inferred from a sample — this engine declares no schema, so this list is what was seen, not what is guaranteed."
    }
}

// MARK: - columns

private struct ColumnsSection: View {
    let detail: SchemaDetail
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            SectionHeader(
                title: detail.inferred ? "FIELDS" : "COLUMNS",
                count: detail.columns?.count)

            if let columns = detail.columns {
                if columns.isEmpty {
                    SchemaNote("This object declares no columns.")
                } else {
                    LazyVStack(spacing: 0) {
                        ForEach(Array(columns.enumerated()), id: \.element.id) { i, col in
                            ColumnRow(column: col, striped: i.isMultiple(of: 2))
                        }
                    }
                }
            } else {
                SchemaNote("This engine declares no schema for \(detail.kind) objects.")
            }
        }
    }
}

private struct ColumnRow: View {
    let column: SchemaColumn
    let striped: Bool

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Image(systemName: column.isPrimaryKey ? "key.fill" : "circle.fill")
                .font(.system(size: column.isPrimaryKey ? 9 : 3.5))
                .foregroundStyle(
                    column.isPrimaryKey ? Color.accentColor : Color(nsColor: .quaternaryLabelColor)
                )
                .frame(width: 12, alignment: .center)
                .help(column.isPrimaryKey ? "primary key" : "")

            VStack(alignment: .leading, spacing: 1) {
                Text(column.name)
                    .font(.system(size: 11.5))
                    .foregroundStyle(Color.primary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                if let d = column.defaultValue {
                    Text("default \(d)")
                        .font(.system(size: 9, design: .monospaced))
                        .foregroundStyle(.tertiary)
                        .lineLimit(1)
                }
            }

            Spacer(minLength: 6)

            if let nullable = column.nullable {
                Text(nullable ? "NULL" : "NOT NULL")
                    .font(.system(size: 8.5, weight: .medium))
                    .foregroundStyle(nullable ? Color.secondary : Color.primary.opacity(0.65))
                    .opacity(nullable ? 0.6 : 1)
            }

            Text(column.displayType)
                .font(.system(size: 10.5, design: .monospaced))
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)
                .frame(minWidth: 52, alignment: .trailing)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 3.5)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(striped ? Color.primary.opacity(0.035) : Color.clear)
        .contentShape(Rectangle())
        .help(tooltip)
        .contextMenu {
            Button("Copy Name") { copy(column.name) }
            Button("Copy Name and Type") { copy("\(column.name) \(column.displayType)") }
        }
    }

    private var tooltip: String {
        var parts = ["\(column.name) \(column.displayType)"]
        if let l = column.typeDetail { parts.append("logical: \(l)") }
        if let n = column.nullable { parts.append(n ? "nullable" : "not null") }
        if column.isPrimaryKey { parts.append("primary key") }
        if column.isUnique { parts.append("unique") }
        if column.isIndexed { parts.append("indexed") }
        if column.isAutoGenerated { parts.append("auto-generated") }
        if let p = column.presence {
            parts.append("present in \(Int((p * 100).rounded()))% of sampled documents")
        }
        return parts.joined(separator: " · ")
    }

    private func copy(_ s: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(s, forType: .string)
    }
}

// MARK: - indexes

private struct IndexesSection: View {
    let detail: SchemaDetail

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            SectionHeader(
                title: "INDEXES", count: detail.indexesReported ? detail.indexes.count : nil)

            if !detail.indexesReported {
                SchemaNote("Indexes not reported for this object.")
            } else if detail.indexes.isEmpty {
                SchemaNote("No indexes.")
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(detail.indexes.enumerated()), id: \.element.id) { i, index in
                        IndexRow(index: index, striped: i.isMultiple(of: 2))
                    }
                }
            }
        }
    }
}

private struct IndexRow: View {
    let index: SchemaIndex
    let striped: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Image(systemName: index.isPrimary ? "key.fill" : "list.bullet.indent")
                    .font(.system(size: 9))
                    .foregroundStyle(index.isPrimary ? Color.accentColor : Color.secondary)
                    .frame(width: 12)
                Text(index.name)
                    .font(.system(size: 11))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 4)
                ForEach(badges, id: \.self) { b in Badge(b) }
            }
            if !index.keys.isEmpty {
                Text(index.keyLine)
                    .font(.system(size: 10, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .padding(.leading, 18)
            }
            if let p = index.partial, !p.isEmpty {
                Text("WHERE \(p)")
                    .font(.system(size: 9, design: .monospaced))
                    .foregroundStyle(.tertiary)
                    .lineLimit(2)
                    .padding(.leading, 18)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 4)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(striped ? Color.primary.opacity(0.035) : Color.clear)
        .contextMenu {
            Button("Copy Index Name") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(index.name, forType: .string)
            }
        }
    }

    private var badges: [String] {
        var out: [String] = []
        if index.isPrimary { out.append("PK") }
        if index.isUnique { out.append("UNIQUE") }
        if index.partial != nil { out.append("PARTIAL") }
        if let ttl = index.ttlSeconds { out.append("TTL \(ttl)s") }
        if let t = index.type, !t.isEmpty, t.caseInsensitiveCompare("btree") != .orderedSame {
            out.append(t.uppercased())
        }
        return out
    }
}

private struct Badge: View {
    let text: String
    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text)
            .font(.system(size: 8, weight: .semibold))
            .foregroundStyle(.secondary)
            .padding(.horizontal, 4)
            .padding(.vertical, 1)
            .background(
                Capsule().fill(Color(nsColor: .quaternaryLabelColor).opacity(0.5)))
    }
}

// MARK: - engine extras

/// Whatever the driver attached that has no column or index to hang off.
private struct ExtraSection: View {
    let detail: SchemaDetail

    var body: some View {
        if !detail.extra.isEmpty {
            VStack(alignment: .leading, spacing: 0) {
                SectionHeader(title: "ENGINE DETAILS", count: nil)
                VStack(spacing: 0) {
                    ForEach(Array(detail.extra.enumerated()), id: \.offset) { i, pair in
                        HStack(alignment: .firstTextBaseline, spacing: 8) {
                            Text(pair.key.replacingOccurrences(of: "_", with: " "))
                                .font(.system(size: 10))
                                .foregroundStyle(.secondary)
                            Spacer(minLength: 6)
                            Text(pair.value)
                                .font(.system(size: 10, design: .monospaced))
                                .foregroundStyle(Color.primary)
                                .lineLimit(1)
                                .truncationMode(.middle)
                                .textSelection(.enabled)
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 3)
                        .background(i.isMultiple(of: 2) ? Color.primary.opacity(0.035) : .clear)
                    }
                }
            }
        }
    }
}

// MARK: - shared bits

private struct SectionHeader: View {
    let title: String
    let count: Int?

    var body: some View {
        HStack(spacing: 5) {
            Text(title)
                .font(.system(size: 9, weight: .semibold))
                .tracking(0.5)
                .foregroundStyle(.tertiary)
            if let count {
                Text("\(count)")
                    .font(.system(size: 9, weight: .semibold))
                    .monospacedDigit()
                    .foregroundStyle(.quaternary)
            }
            Spacer()
        }
        .padding(.horizontal, 12)
        .padding(.bottom, 4)
    }
}

private struct SchemaNote: View {
    let text: String
    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text)
            .font(.system(size: 10.5))
            .foregroundStyle(.tertiary)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, 12)
            .padding(.vertical, 2)
    }
}

private struct SchemaMessage: View {
    let symbol: String
    let text: String

    var body: some View {
        VStack(spacing: 8) {
            Spacer()
            Image(systemName: symbol)
                .font(.system(size: 26))
                .foregroundStyle(.tertiary)
            Text(text)
                .font(.caption)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 20)
            Spacer()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

// MARK: - copy

private struct CopyMenu: View {
    let detail: SchemaDetail
    @ObservedObject var model: AppModel

    var body: some View {
        Menu {
            Button("Copy Column Names") { copy(SchemaClipboard.names(detail), "column names") }
            Button("Copy Names and Types") {
                copy(SchemaClipboard.namesAndTypes(detail), "columns with types")
            }
            Button("Copy as SELECT List") {
                copy(SchemaClipboard.selectList(detail), "SELECT list")
            }
            .help("Comma-separated, ready to paste after SELECT")
            Divider()
            if let ddl = detail.definition, !ddl.isEmpty {
                Button("Copy Definition (from the engine)") { copy(ddl, "definition") }
            } else if let generated = SchemaClipboard.generatedDDL(detail) {
                Button("Copy CREATE TABLE (generated)") { copy(generated, "generated DDL") }
            }
            Button("Copy Raw describe() JSON") { copy(detail.rawJSON, "describe JSON") }
        } label: {
            Image(systemName: "doc.on.doc")
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .disabled(detail.columns == nil && detail.extra.isEmpty)
        .help("Copy the column list or a CREATE statement")
    }

    private func copy(_ text: String, _ label: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
        model.message = "\(label) copied"
        model.isError = false
    }
}
