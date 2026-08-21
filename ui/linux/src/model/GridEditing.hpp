// GridEditing.hpp — every edit typed into the grid and not yet committed.

// Addresses are captured at staging; refreshing expect at commit would compare the server against itself.
#ifndef DATAGREP_GRID_EDITING_HPP
#define DATAGREP_GRID_EDITING_HPP

#include "Mutation.hpp"

#include <QHash>
#include <QObject>
#include <QString>
#include <QVector>

#include <optional>

namespace dg {

struct StagedState {
    enum class Kind {
        Pending,
        Applied,
        // The document changed on the server, so the guard refused the write.
        Conflicted,
        Failed,
        // The batch halted before this one: still pending, nothing written.
        NotAttempted,
    };
    Kind kind = Kind::Pending;
    QString message;  // for Conflicted / Failed

    bool isDone() const { return kind == Kind::Applied; }
};

struct StagedField {
    QString field;
    MutationValue value;
    std::optional<MutationValue> loaded;
};

// One document's staged changes, addressed the way the engine addresses it.
struct StagedDocument {
    QString id;
    QVector<FieldValue> key;
    QVector<FieldValue> expect;
    int row = 0;

    QVector<StagedField> sets;  // ordered by first edit
    bool isDelete = false;
    StagedState state;

    bool isPending() const { return !state.isDone(); }
    bool isConflicted() const { return state.kind == StagedState::Kind::Conflicted; }

    std::optional<MutationValue> valueOf(const QString& field) const;

    DocumentMutation mutation() const;
    DocumentAddress address() const;
};

class PendingEdits : public QObject {
    Q_OBJECT

public:
    explicit PendingEdits(QObject* parent = nullptr);

    const QVector<StagedDocument>& documents() const { return documents_; }
    QVector<StagedDocument> pending() const;
    int pendingCount() const;
    bool isEmpty() const { return documents_.isEmpty(); }
    int deleteCount() const;
    int updateCount() const;

    // Documents whose last commit the guard refused — still staged.
    QVector<StagedDocument> conflicted() const;
    int conflictCount() const;

    const StagedDocument* documentAtRow(int row) const;
    std::optional<MutationValue> value(int row, const QString& field) const;
    bool isDeleted(int row) const;

    void stage(const QString& id, int row, const QVector<FieldValue>& key,
               const QVector<FieldValue>& expect, const QString& field,
               const MutationValue& value,
               const std::optional<MutationValue>& loaded);

    // Field edits are kept, not dropped: undoing the delete gives them back.
    void stageDelete(const QString& id, int row, const QVector<FieldValue>& key,
                     const QVector<FieldValue>& expect);

    void unstage(int row, const QString& field);

    void discardRow(int row);
    void discardAll();

    std::optional<int> rebase(const QString& id, const QVector<FieldValue>& expect);

    std::optional<int> discardById(const QString& id);

    // Matched by position; a report that does not line up one-for-one folds nothing in.
    bool apply(const MutationReport& report, const QStringList& committedIds);

signals:
    // Something was staged, discarded or resolved; the bar redraws from this.
    void stagingChanged();

private:
    StagedDocument existing(const QString& id, int row,
                            const QVector<FieldValue>& key,
                            const QVector<FieldValue>& expect) const;
    void put(const StagedDocument& doc, int row);
    int indexOf(const QString& id) const;

    QVector<StagedDocument> documents_;
    // Grid row -> document id, so per-cell "is this row staged?" is O(1).
    QHash<int, QString> rowIndex_;
};

}  // namespace dg

#endif  // DATAGREP_GRID_EDITING_HPP
