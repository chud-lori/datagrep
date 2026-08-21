import DatagrepKit
import SwiftUI

/// A version conflict, as three readings of the same document.
///
/// A 409 is the guard doing its job, and the only useful thing to say about it
/// is *what changed*. So the document is read back and put next to what was
/// loaded and what was typed — three columns, one row per edited field — and
/// the two honest offers are made from there: re-apply my edits onto this
/// version (*rebase*), or drop them (*discard mine*).
///
/// What is deliberately not offered anywhere: retrying the write as it stood.
/// That is the clobber the guard exists to prevent, and it is not any less of
/// one for being one click away.

// MARK: - the review, built once

/// One edited field's three readings.
struct ConflictField: Identifiable {
    let name: String
    /// What the cell held when it was typed into.
    let loaded: MutationValue?
    /// What the server holds now.
    let server: ServerValue
    /// What a rebase would write.
    let typed: MutationValue

    var id: String { name }

    /// The server moved this field, not just the document around it — so a
    /// rebase overwrites somebody else's value here, and the row says so.
    var movedUnderneath: Bool {
        switch server {
        case .value(let now): return now != loaded
        case .nested: return true
        case .missing: return loaded != nil
        }
    }

    var loadedDisplay: String { loaded?.display ?? "—" }
}

/// One conflicted document, ready to review. Every value it shows was computed
/// when the re-read landed: the view does no lookups and no arithmetic, so
/// there is nothing in a button action that can throw.
struct ConflictDocument: Identifiable {
    /// The staged document's own id, which is how a resolution finds it again.
    let id: String
    /// The document's identity, spelled the way the engine spells it.
    let title: String
    let fields: [ConflictField]
    let isDelete: Bool
    /// The guard values a rebase would re-guard against, or nil when the
    /// re-read did not bring back a usable one — a document that is gone, one
    /// the engine could not read, or an engine whose guard is not in the
    /// envelope. Rebase is not offered without it rather than sent unguarded.
    let rebaseGuard: [(field: String, value: MutationValue)]?
    /// The document is no longer on the server.
    let gone: Bool
    /// Why this one could not be read at all, when it could not.
    let error: String?

    var canRebase: Bool { rebaseGuard != nil }

    /// Fields somebody else moved. Rebasing overwrites exactly these.
    var contested: [ConflictField] { fields.filter(\.movedUnderneath) }
}

/// Every conflicted document from one commit, with what the server holds now.
struct ConflictReview {
    let documents: [ConflictDocument]

    var isEmpty: Bool { documents.isEmpty }

    /// Build the review from the staged documents and the re-read that answers
    /// them — matched **by position**, which is the contract
    /// `datagrep_reread_documents` states: one entry per address, in order. The
    /// caller checks the two counts agree before getting here.
    init(
        conflicted: [StagedDocument], server: [ServerDocument], editable: EditableResult
    ) {
        documents = zip(conflicted, server).map { staged, now in
            let fields = staged.sets.map { set in
                ConflictField(
                    name: set.field,
                    loaded: set.loaded,
                    server: now.fields[set.field] ?? .missing,
                    typed: set.value)
            }
            // The fresh guard, read out of the envelope by the field names the
            // engine named — this layer never learns what `_seq_no` is.
            var rebaseGuard: [(field: String, value: MutationValue)]? = []
            for field in editable.guardFields {
                guard let value = now.envelope[field]?.mutationValue else {
                    rebaseGuard = nil
                    break
                }
                rebaseGuard?.append((field, value))
            }
            if !now.found || rebaseGuard?.isEmpty == true { rebaseGuard = nil }
            return ConflictDocument(
                id: staged.id,
                title: Self.title(of: staged),
                fields: fields,
                isDelete: staged.isDelete,
                rebaseGuard: rebaseGuard,
                gone: !now.found && now.error == nil,
                error: now.error)
        }
    }

    private init(documents: [ConflictDocument]) { self.documents = documents }

    /// The review with one document resolved out of it.
    func removing(_ id: String) -> ConflictReview {
        ConflictReview(documents: documents.filter { $0.id != id })
    }

    /// The document's identity as one line — the engine's own field names and
    /// values, joined, rather than a guess at which one is "the id".
    private static func title(of staged: StagedDocument) -> String {
        staged.key.map { "\($0.field)=\($0.value.display)" }.joined(separator: "  ")
    }
}

// MARK: - the sheet

/// The three-column conflict view.
struct ConflictReviewSheet: View {
    @ObservedObject var model: AppModel
    let review: ConflictReview

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    ForEach(review.documents) { document in
                        ConflictDocumentView(model: model, document: document)
                    }
                }
                .padding(14)
            }
            Divider()
            footer
        }
        .frame(width: 720, height: 480)
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(
                review.documents.count == 1
                    ? "1 document changed after you loaded it"
                    : "\(review.documents.count) documents changed after you loaded them"
            )
            .font(.system(size: 14, weight: .semibold))
            Text(
                "Nothing was written for these. Each one is shown as you loaded it, as the server holds it now, and as you typed it — so you can re-apply your edits onto the current version, or drop them."
            )
            .font(.system(size: 11.5))
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(14)
    }

    private var footer: some View {
        HStack {
            Text("Anything left unresolved stays staged and unwritten.")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
            Spacer()
            Button("Close") { model.showConflictReview = false }
                .keyboardShortcut(.defaultAction)
        }
        .padding(12)
    }
}

/// One document's block: its identity, its three columns, and its two choices.
private struct ConflictDocumentView: View {
    @ObservedObject var model: AppModel
    let document: ConflictDocument

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Image(systemName: "arrow.triangle.branch")
                    .foregroundStyle(.orange)
                    .imageScale(.small)
                Text(document.title)
                    .font(.system(size: 11.5, design: .monospaced))
                if document.isDelete {
                    Text("staged for deletion")
                        .font(.system(size: 10, weight: .semibold))
                        .foregroundStyle(.red)
                }
                Spacer(minLength: 6)
            }

            if let error = document.error {
                note(error, warning: true)
            } else if document.gone {
                note(
                    "This document is no longer on the server — somebody deleted it. There is no version to re-apply your edits onto.",
                    warning: true)
            }

            if !document.fields.isEmpty {
                columns
            } else if document.isDelete {
                note(
                    "A delete has no fields of its own. Re-applying it means deleting whatever the document is now, including the change somebody else just made.",
                    warning: false)
            }

            if !document.gone && document.error == nil {
                summary
            }
            choices
            Divider()
        }
    }

    private var columns: some View {
        VStack(spacing: 0) {
            HStack(spacing: 0) {
                columnHead("field", width: 150)
                columnHead("you loaded", width: 170)
                columnHead("on the server now", width: 170)
                columnHead("you typed", width: 170)
            }
            .padding(.bottom, 3)
            Divider()
            ForEach(document.fields) { field in
                ConflictFieldRow(field: field)
            }
        }
    }

    private func columnHead(_ text: String, width: CGFloat) -> some View {
        Text(text)
            .font(.system(size: 10, weight: .semibold))
            .foregroundStyle(.secondary)
            .frame(width: width, alignment: .leading)
    }

    /// The one sentence that decides which button is right.
    private var summary: some View {
        let contested = document.contested.count
        let text: String
        if contested == 0 {
            text =
                "The fields you edited are unchanged — somebody changed this document elsewhere. Re-applying writes your edits onto their version and overwrites nothing of theirs."
        } else if contested == 1 {
            text =
                "1 of the fields you edited was changed by somebody else. Re-applying overwrites their value with yours."
        } else {
            text =
                "\(contested) of the fields you edited were changed by somebody else. Re-applying overwrites their values with yours."
        }
        return note(text, warning: contested > 0)
    }

    private var choices: some View {
        HStack(spacing: 8) {
            Spacer(minLength: 6)
            Button("Discard Mine") { model.discardConflicted(document) }
                .buttonStyle(.bordered)
                .controlSize(.small)
            Button(document.isDelete ? "Delete It Anyway" : "Re-apply Onto This Version") {
                model.rebaseConflicted(document)
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.small)
            .disabled(!document.canRebase)
        }
    }

    private func note(_ text: String, warning: Bool) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Image(systemName: warning ? "exclamationmark.triangle.fill" : "info.circle.fill")
                .foregroundStyle(warning ? Color.orange : Color.secondary)
                .imageScale(.small)
            Text(text)
                .font(.system(size: 11))
                .foregroundStyle(warning ? .primary : .secondary)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 4)
        }
    }
}

/// One field, three ways. The middle column is tinted when it moved: that is
/// the whole reason this view exists, and it should be findable at a glance.
private struct ConflictFieldRow: View {
    let field: ConflictField

    var body: some View {
        HStack(spacing: 0) {
            Text(field.name)
                .font(.system(size: 11, weight: .medium, design: .monospaced))
                .frame(width: 150, alignment: .leading)
                .lineLimit(1)
                .truncationMode(.middle)
            cell(field.loadedDisplay, tint: .secondary)
            cell(field.server.display, tint: field.movedUnderneath ? .orange : .secondary)
            cell(field.typed.display, tint: .primary)
        }
        .padding(.vertical, 3)
    }

    private func cell(_ text: String, tint: Color) -> some View {
        Text(text)
            .font(.system(size: 11, design: .monospaced))
            .foregroundStyle(tint)
            .frame(width: 170, alignment: .leading)
            .lineLimit(1)
            .truncationMode(.tail)
            .help(text)
    }
}
