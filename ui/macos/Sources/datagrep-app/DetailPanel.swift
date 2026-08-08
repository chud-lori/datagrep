import AppKit
import DatagrepKit
import SwiftUI

/// The inspector on the right of the results grid. It has two modes and one
/// switch between them.
///
/// * **Schema** — what the selected table/collection/key *is*: columns, indexes,
///   stats. Driven by `datagrep_catalog_describe_json`; see `SchemaPane`.
/// * **Cell** — what one nested value *contains*. A nested cell
///   (`datagrep_rows_cell_kind == 3`) draws as a chip like `{4 fields}`; clicking
///   it asks the ABI for `datagrep_rows_cell_detail_json` and shows the whole
///   value here, pretty-printed.
///
/// The switch is non-destructive on purpose: each mode keeps its own state, so
/// flipping to the schema and back does not throw away the cell you were
/// reading, and it never re-issues a load.
struct DetailPanel: View {
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            modeSwitch
            Divider()
            if model.inspectorMode == .schema {
                SchemaPane(model: model)
            } else {
                CellDetailPane(model: model)
            }
        }
        // A material, not a flat fill: the inspector reads as a floating
        // layer above the results pane, matching the status bar treatment.
        .background(.ultraThinMaterial)
    }

    /// Always both segments, always visible. A switch that appears and
    /// disappears with content is a switch the user cannot learn.
    private var modeSwitch: some View {
        Picker("", selection: $model.inspectorMode) {
            Label("Schema", systemImage: "tablecells").tag(InspectorMode.schema)
            Label("Cell", systemImage: "curlybraces").tag(InspectorMode.cell)
        }
        .pickerStyle(.segmented)
        .labelsHidden()
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
    }
}

private struct CellDetailPane: View {
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()

            if model.detailBody.isEmpty {
                emptyState
            } else {
                ScrollView([.vertical, .horizontal]) {
                    Text(model.detailBody)
                        .font(.system(size: 11, design: .monospaced))
                        .textSelection(.enabled)
                        .foregroundStyle(Color.primary)
                        .padding(12)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }

            Divider()
            legend
        }
    }

    private var header: some View {
        HStack(spacing: 8) {
            Image(systemName: "curlybraces")
                .foregroundStyle(Color.accentColor)
            VStack(alignment: .leading, spacing: 1) {
                Text("Cell detail").font(.headline)
                Text(model.detailTitle.isEmpty ? "nothing selected" : model.detailTitle)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 6)
            Button {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(model.detailBody, forType: .string)
                model.message = "cell JSON copied"
                model.isError = false
            } label: {
                Image(systemName: "doc.on.doc")
            }
            .buttonStyle(.borderless)
            .disabled(model.detailBody.isEmpty)
            .help("copy this JSON")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
    }

    private var emptyState: some View {
        VStack(spacing: 8) {
            Spacer()
            Image(systemName: "square.on.square.dashed")
                .font(.system(size: 26))
                .foregroundStyle(.tertiary)
            Text("Click a `{…}` chip in the grid to see the whole value.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 20)
            Spacer()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    /// The product's honesty claim, spelled out where it can be read: a field
    /// missing from a document is a different fact from a field that is null,
    /// and both are different from an empty string.
    private var legend: some View {
        VStack(alignment: .leading, spacing: 5) {
            Text("CELL KINDS")
                .font(.system(size: 9, weight: .semibold))
                .foregroundStyle(.tertiary)
            LegendRow(sample: .null, title: "NULL", note: "present, and null")
            LegendRow(sample: .empty, title: "", note: "present, empty string")
            LegendRow(sample: .absent, title: "—", note: "ABSENT: not in the document at all")
            LegendRow(sample: .nested, title: "{n fields}", note: "nested — click to open here")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct LegendRow: View {
    enum Sample { case null, empty, absent, nested }
    let sample: Sample
    let title: String
    let note: String

    var body: some View {
        HStack(spacing: 8) {
            Group {
                switch sample {
                case .null:
                    Text("NULL").italic().foregroundStyle(.secondary)
                case .empty:
                    Rectangle()
                        .fill(Color(nsColor: .quaternaryLabelColor))
                        .frame(width: 22, height: 1)
                        .frame(maxHeight: .infinity, alignment: .bottom)
                        .padding(.bottom, 2)
                case .absent:
                    Text("—").foregroundStyle(.tertiary)
                case .nested:
                    Text(title)
                        .foregroundStyle(Color.accentColor)
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(
                            RoundedRectangle(cornerRadius: 4)
                                .fill(Color.accentColor.opacity(0.14)))
                }
            }
            .font(.system(size: 10, design: .monospaced))
            .frame(width: 78, height: 14, alignment: .leading)

            Text(note)
                .font(.system(size: 10))
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.tail)
        }
    }
}
