import DatagrepKit
import SwiftUI

/// The bar under the grid while edits are staged.
struct StagedEditsBar: View {
    @ObservedObject var model: AppModel
    @ObservedObject var edits: PendingEdits

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "square.and.pencil")
                .foregroundStyle(Color.accentColor)
                .imageScale(.medium)

            VStack(alignment: .leading, spacing: 1) {
                Text(headline)
                    .font(.system(size: 12, weight: .semibold))
                if let detail {
                    Text(detail)
                        .font(.system(size: 10.5))
                        .foregroundStyle(.secondary)
                }
            }

            Spacer(minLength: 8)

            if model.isCommitting {
                ProgressView()
                    .controlSize(.small)
                Text("committing…")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
            } else if edits.pendingCount == 0 {
                Button("Reload") { model.reloadResult() }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
            } else {
                Button("Discard") { model.discardStagedEdits() }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
                if edits.conflictCount > 0 {
                    Button(conflictTitle) { model.reviewConflicts() }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.small)
                        .tint(.orange)
                        .disabled(model.isRereading)
                }
                Button(commitTitle) { model.commitStagedEdits() }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .background(.ultraThinMaterial)
        .overlay(alignment: .top) {
            Rectangle().fill(Color.accentColor.opacity(0.35)).frame(height: 1)
        }
    }

    private var headline: String {
        let n = edits.pendingCount
        if n == 0 {
            let written = edits.documents.count
            return written == 1
                ? "1 document written — the grid still shows what was loaded"
                : "\(written) documents written — the grid still shows what was loaded"
        }
        return n == 1
            ? "1 document edited, not yet written"
            : "\(n) documents edited, not yet written"
    }

    private var detail: String? {
        var parts: [String] = []
        if edits.updateCount > 0 { parts.append("\(edits.updateCount) to update") }
        if edits.deleteCount > 0 { parts.append("\(edits.deleteCount) to delete") }
        let applied = edits.documents.count - edits.pendingCount
        if applied > 0 { parts.append("\(applied) already written") }
        if edits.conflictCount > 0 {
            parts.append(
                edits.conflictCount == 1
                    ? "1 changed on the server" : "\(edits.conflictCount) changed on the server")
        }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    private var commitTitle: String {
        edits.pendingCount == 1 ? "Commit 1…" : "Commit \(edits.pendingCount)…"
    }

    private var conflictTitle: String {
        edits.conflictCount == 1 ? "Resolve 1 Conflict…" : "Resolve \(edits.conflictCount) Conflicts…"
    }
}

// MARK: - the report

/// What the commit actually did, per document.
struct MutationReportSheet: View {
    @ObservedObject var model: AppModel
    let report: MutationReport

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 10) {
                    if !report.notices.isEmpty { notices }
                    ForEach(report.rows) { row in
                        MutationRowLine(row: row)
                    }
                }
                .padding(14)
            }
            Divider()
            footer
        }
        .frame(width: 560, height: 420)
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.system(size: 14, weight: .semibold))
            Text(subtitle)
                .font(.system(size: 11.5))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(14)
    }

    private var title: String {
        if report.isClean {
            return report.applied == 1
                ? "1 document written" : "\(report.applied) documents written"
        }
        return "The batch stopped part way through"
    }

    private var subtitle: String {
        var parts = ["\(report.applied) applied"]
        if report.failed > 0 { parts.append("\(report.failed) failed") }
        if report.notAttempted > 0 { parts.append("\(report.notAttempted) never attempted") }
        var line = parts.joined(separator: " · ")
        if report.notAttempted > 0 {
            line +=
                ". The ones that were never attempted are still staged — nothing was written for them, and nothing was lost."
        }
        if report.conflicts > 0 {
            line +=
                " A version conflict means the document changed on the server after you loaded it, so the write was refused rather than overwriting someone else's change. What you typed is still staged — resolve it below to see what changed."
        }
        return line
    }

    private var notices: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(report.notices) { notice in
                HStack(alignment: .firstTextBaseline, spacing: 7) {
                    Image(
                        systemName: notice.isWarning
                            ? "exclamationmark.triangle.fill" : "info.circle.fill"
                    )
                    .foregroundStyle(notice.isWarning ? Color.orange : Color.secondary)
                    .imageScale(.small)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(notice.message)
                            .font(.system(size: 11.5))
                            .fixedSize(horizontal: false, vertical: true)
                        if let code = notice.code {
                            Text(code)
                                .font(.system(size: 10, design: .monospaced))
                                .foregroundStyle(.tertiary)
                        }
                    }
                }
            }
            Divider().padding(.vertical, 2)
        }
    }

    private var footer: some View {
        HStack {
            Text(footerNote)
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
            Spacer()
            if report.conflicts > 0 {
                Button("Resolve Conflicts…") { model.reviewConflicts() }
                    .disabled(model.isRereading)
            }
            Button("Done") { model.showMutationReport = false }
                .keyboardShortcut(.defaultAction)
        }
        .padding(12)
    }

    private var footerNote: String {
        if report.conflicts > 0 {
            return "Reads each conflicted document back and shows what changed."
        }
        return report.isClean ? "" : "Re-run the statement to see what the server holds now."
    }
}

/// One document's line in the report.
private struct MutationRowLine: View {
    let row: MutationRow

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 9) {
            Image(systemName: symbol)
                .foregroundStyle(tint)
                .imageScale(.small)
                .frame(width: 14)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(row.op)
                        .font(.system(size: 10, weight: .semibold))
                        .foregroundStyle(.secondary)
                    Text("\(row.index)/\(row.documentID)")
                        .font(.system(size: 11.5, design: .monospaced))
                }
                if let detail {
                    Text(detail)
                        .font(.system(size: 11))
                        .foregroundStyle(row.outcome == .applied ? .secondary : .primary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 6)
        }
    }

    private var symbol: String {
        switch row.outcome {
        case .applied: return "checkmark.circle.fill"
        case .failed: return row.conflict ? "arrow.triangle.branch" : "xmark.circle.fill"
        case .notAttempted: return "clock"
        }
    }

    private var tint: Color {
        switch row.outcome {
        case .applied: return .green
        case .failed: return row.conflict ? .orange : .red
        case .notAttempted: return .secondary
        }
    }

    private var detail: String? {
        switch row.outcome {
        case .applied:
            var line = "written"
            if let seq = row.seqNo { line += " · now at _seq_no \(seq)" }
            if row.forcedRefresh {
                line += " · the server forced an immediate refresh rather than waiting for one"
            }
            return line
        case .notAttempted:
            return "never attempted — the batch stopped before it, so this is still staged"
        case .failed:
            if row.conflict {
                return
                    "version conflict — this document changed on the server after you loaded it, so nothing was written"
            }
            return row.error ?? "the write failed"
        }
    }
}
