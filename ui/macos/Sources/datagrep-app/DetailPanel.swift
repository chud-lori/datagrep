import AppKit
import DatagrepKit
import SwiftUI

/// The inspector on the right of the results grid.
///
/// A nested cell (`datagrep_rows_cell_kind == 3`) draws as a chip like `{4 fields}`;
/// clicking the chip asks the ABI for `datagrep_rows_cell_detail_json` and shows the
/// whole value here, pretty-printed. The grid never holds this text — it is
/// fetched for the one cell that was clicked, and dropped when the panel closes.
struct DetailPanel: View {
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
        .background(Color(nsColor: .controlBackgroundColor))
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
