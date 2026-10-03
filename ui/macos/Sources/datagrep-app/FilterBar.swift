import DatagrepKit
import SwiftUI

struct FilterDraftRow: Identifiable, Equatable {
    let id = UUID()
    var filter: RowFilter

    init(_ filter: RowFilter) { self.filter = filter }
}

/// Column / operator / value rows above the grid; Apply re-runs the statement with their WHERE.
struct FilterBar: View {
    @ObservedObject var model: AppModel

    var body: some View {
        let columns = model.filterColumns
        let operators = model.filterOperators
        VStack(alignment: .leading, spacing: 5) {
            ForEach($model.filterDraft) { $row in
                FilterRowView(
                    row: $row, columns: columns, operators: operators,
                    submit: model.applyFilters,
                    remove: { model.removeFilterRow(row.id) })
            }
            HStack(spacing: 8) {
                Button {
                    model.addFilterRow()
                } label: {
                    Label("Add Filter", systemImage: "plus")
                }
                .help("Add another condition — every condition must hold")
                Spacer()
                Button("Clear") { model.clearDerived() }
                    .disabled(!model.hasDerivedClauses && model.filterDraft.isEmpty)
                    .help("Remove every filter and the header sort")
                Button("Apply") { model.applyFilters() }
                    .disabled(model.isRunning)
                    .help("Re-run the statement with these conditions as its WHERE  ↩")
                Button {
                    model.closeFilterBar()
                } label: {
                    Image(systemName: "xmark")
                }
                .buttonStyle(.borderless)
                .help("Close the filter bar and drop its filters")
            }
            .controlSize(.small)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .background(Color(nsColor: .controlBackgroundColor))
    }
}

private struct FilterRowView: View {
    @Binding var row: FilterDraftRow
    let columns: [String]
    let operators: [FilterOperator]
    let submit: () -> Void
    let remove: () -> Void

    private var needsValue: Bool {
        operators.first { $0.op == row.filter.op }?.needsValue ?? true
    }

    var body: some View {
        HStack(spacing: 6) {
            Picker("Column", selection: $row.filter.column) {
                ForEach(columns, id: \.self) { Text($0).tag($0) }
            }
            .labelsHidden()
            .frame(minWidth: 110, maxWidth: 200)
            Picker("Operator", selection: $row.filter.op) {
                ForEach(operators, id: \.op) { Text($0.label).tag($0.op) }
            }
            .labelsHidden()
            .fixedSize()
            if needsValue {
                TextField("value", text: $row.filter.value)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit(submit)
            } else {
                Spacer()
            }
            Button(action: remove) {
                Image(systemName: "minus.circle")
            }
            .buttonStyle(.borderless)
            .help("Remove this condition")
        }
        .controlSize(.small)
    }
}
