#include "GridEditing.hpp"

namespace dg {

std::optional<MutationValue> StagedDocument::valueOf(const QString& field) const {
    for (const StagedField& set : sets) {
        if (set.field == field) {
            return set.value;
        }
    }
    return std::nullopt;
}

DocumentMutation StagedDocument::mutation() const {
    DocumentMutation m;
    // `path` is where a NEW document would go; nothing here inserts one, so it
    // is sent empty rather than guessed.
    m.key = key;
    m.expect = expect;
    if (!isDelete) {
        for (const StagedField& set : sets) {
            m.sets.append(FieldValue{set.field, set.value});
        }
    }
    m.isDelete = isDelete;
    return m;
}

DocumentAddress StagedDocument::address() const {
    // A re-read addresses a document with the very same key its write did.
    return DocumentAddress{key};
}

PendingEdits::PendingEdits(QObject* parent) : QObject(parent) {}

QVector<StagedDocument> PendingEdits::pending() const {
    QVector<StagedDocument> out;
    for (const StagedDocument& d : documents_) {
        if (d.isPending()) {
            out.append(d);
        }
    }
    return out;
}

int PendingEdits::pendingCount() const { return pending().size(); }

int PendingEdits::deleteCount() const {
    int n = 0;
    for (const StagedDocument& d : documents_) {
        if (d.isPending() && d.isDelete) {
            ++n;
        }
    }
    return n;
}

int PendingEdits::updateCount() const {
    int n = 0;
    for (const StagedDocument& d : documents_) {
        if (d.isPending() && !d.isDelete) {
            ++n;
        }
    }
    return n;
}

QVector<StagedDocument> PendingEdits::conflicted() const {
    QVector<StagedDocument> out;
    for (const StagedDocument& d : documents_) {
        if (d.isConflicted()) {
            out.append(d);
        }
    }
    return out;
}

int PendingEdits::conflictCount() const { return conflicted().size(); }

const StagedDocument* PendingEdits::documentAtRow(int row) const {
    const auto it = rowIndex_.constFind(row);
    if (it == rowIndex_.constEnd()) {
        return nullptr;
    }
    const int at = indexOf(it.value());
    return at >= 0 ? &documents_[at] : nullptr;
}

std::optional<MutationValue> PendingEdits::value(int row, const QString& field) const {
    const StagedDocument* doc = documentAtRow(row);
    return doc != nullptr ? doc->valueOf(field) : std::nullopt;
}

bool PendingEdits::isDeleted(int row) const {
    const StagedDocument* doc = documentAtRow(row);
    return doc != nullptr && doc->isDelete;
}

void PendingEdits::stage(const QString& id, int row, const QVector<FieldValue>& key,
                         const QVector<FieldValue>& expect, const QString& field,
                         const MutationValue& value,
                         const std::optional<MutationValue>& loaded) {
    StagedDocument doc = existing(id, row, key, expect);
    bool found = false;
    for (StagedField& set : doc.sets) {
        if (set.field == field) {
            // Retyping keeps the loaded value it was FIRST typed over.
            set.value = value;
            found = true;
            break;
        }
    }
    if (!found) {
        doc.sets.append(StagedField{field, value, loaded});
    }
    // A row edited again after a failed commit is pending again; leaving it
    // marked failed would report a stale verdict.
    doc.state = StagedState{};
    put(doc, row);
    emit stagingChanged();
}

void PendingEdits::stageDelete(const QString& id, int row,
                               const QVector<FieldValue>& key,
                               const QVector<FieldValue>& expect) {
    StagedDocument doc = existing(id, row, key, expect);
    doc.isDelete = true;
    doc.state = StagedState{};
    put(doc, row);
    emit stagingChanged();
}

void PendingEdits::unstage(int row, const QString& field) {
    const auto it = rowIndex_.constFind(row);
    if (it == rowIndex_.constEnd()) {
        return;
    }
    const int at = indexOf(it.value());
    if (at < 0) {
        return;
    }
    auto& sets = documents_[at].sets;
    for (int i = sets.size() - 1; i >= 0; --i) {
        if (sets[i].field == field) {
            sets.removeAt(i);
        }
    }
    if (sets.isEmpty() && !documents_[at].isDelete) {
        documents_.removeAt(at);
        rowIndex_.remove(row);
    }
    emit stagingChanged();
}

void PendingEdits::discardRow(int row) {
    const auto it = rowIndex_.constFind(row);
    if (it == rowIndex_.constEnd()) {
        return;
    }
    const int at = indexOf(it.value());
    if (at >= 0) {
        documents_.removeAt(at);
    }
    rowIndex_.remove(row);
    emit stagingChanged();
}

void PendingEdits::discardAll() {
    if (documents_.isEmpty()) {
        return;
    }
    documents_.clear();
    rowIndex_.clear();
    emit stagingChanged();
}

std::optional<int> PendingEdits::rebase(const QString& id,
                                        const QVector<FieldValue>& expect) {
    const int at = indexOf(id);
    if (at < 0) {
        return std::nullopt;
    }
    documents_[at].expect = expect;
    documents_[at].state = StagedState{};
    emit stagingChanged();
    return documents_[at].row;
}

std::optional<int> PendingEdits::discardById(const QString& id) {
    const int at = indexOf(id);
    if (at < 0) {
        return std::nullopt;
    }
    const int row = documents_[at].row;
    documents_.removeAt(at);
    rowIndex_.remove(row);
    emit stagingChanged();
    return row;
}

bool PendingEdits::apply(const MutationReport& report,
                         const QStringList& committedIds) {
    if (report.rows.size() != committedIds.size()) {
        return false;
    }
    for (int i = 0; i < committedIds.size(); ++i) {
        const int at = indexOf(committedIds[i]);
        if (at < 0) {
            continue;
        }
        const MutationRow& row = report.rows[i];
        switch (row.outcome) {
            case MutationRow::Outcome::Applied:
                documents_[at].state = StagedState{StagedState::Kind::Applied, {}};
                break;
            case MutationRow::Outcome::NotAttempted:
                documents_[at].state =
                    StagedState{StagedState::Kind::NotAttempted, {}};
                break;
            case MutationRow::Outcome::Failed: {
                const QString message = row.error.isEmpty()
                                            ? QStringLiteral("the write failed")
                                            : row.error;
                documents_[at].state = StagedState{
                    row.conflict ? StagedState::Kind::Conflicted
                                 : StagedState::Kind::Failed,
                    message};
                break;
            }
        }
    }
    emit stagingChanged();
    return true;
}

StagedDocument PendingEdits::existing(const QString& id, int row,
                                      const QVector<FieldValue>& key,
                                      const QVector<FieldValue>& expect) const {
    const int at = indexOf(id);
    if (at >= 0) {
        return documents_[at];
    }
    StagedDocument doc;
    doc.id = id;
    doc.key = key;
    doc.expect = expect;
    doc.row = row;
    return doc;
}

void PendingEdits::put(const StagedDocument& doc, int row) {
    const int at = indexOf(doc.id);
    if (at >= 0) {
        documents_[at] = doc;
    } else {
        documents_.append(doc);
    }
    rowIndex_.insert(row, doc.id);
}

int PendingEdits::indexOf(const QString& id) const {
    for (int i = 0; i < documents_.size(); ++i) {
        if (documents_[i].id == id) {
            return i;
        }
    }
    return -1;
}

}  // namespace dg
