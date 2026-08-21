// GridEditing.hpp — every edit typed into the grid and not yet committed.
//
// Linux counterpart of the macOS PendingEdits store, same commitments: keyed
// by document identity, not by row (a row number is a position in one result;
// the write is addressed to a document); the address is captured when the edit
// is STAGED, never at commit time — expect values refreshed just before the
// write would compare the server against itself. The one exception is an
// explicit rebase. Cleared wholesale when a new result arrives.

#ifndef DATAGREP_GRID_EDITING_HPP
#define DATAGREP_GRID_EDITING_HPP

#include "Mutation.hpp"

#include <QHash>
#include <QObject>
#include <QString>
#include <QVector>

#include <optional>

namespace dg {

// Where one staged document stands. Everything except Applied is still owed to
// the server — that is what makes a halted batch resumable.
struct StagedState {
    enum class Kind {
        Pending,
        Applied,
        // The document changed on the server after it was loaded, so the guard
        // refused the write. Nothing was written.
        Conflicted,
        Failed,
        // The batch halted before this one. Nothing was written and nothing
        // was lost — still pending, and says so rather than silently reverting.
        NotAttempted,
    };
    Kind kind = Kind::Pending;
    QString message;  // for Conflicted / Failed

    bool isDone() const { return kind == Kind::Applied; }
};

// One field's staged write, with the value it was typed over. The loaded value
// is kept rather than re-read from the grid later: it is half of what a
// version conflict has to show, and the grid can be re-queried underneath it.
struct StagedField {
    QString field;
    MutationValue value;
    // What the cell held when it was typed into; nullopt when the field was
    // not on the row at all — absent and null are different facts.
    std::optional<MutationValue> loaded;
};

// One document's staged changes, addressed the way the engine addresses it.
struct StagedDocument {
    QString id;
    QVector<FieldValue> key;
    QVector<FieldValue> expect;
    // The grid row this was staged from. Display only: rows are re-numbered by
    // the next query; the address above is what a write uses.
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

    // Staging order, which is also commit order. Applied documents stay in the
    // list so their new values keep showing in the grid, but they are not work.
    const QVector<StagedDocument>& documents() const { return documents_; }
    QVector<StagedDocument> pending() const;
    int pendingCount() const;
    bool isEmpty() const { return documents_.isEmpty(); }
    int deleteCount() const;
    int updateCount() const;

    // Documents whose last commit the guard refused — still staged, and what
    // the conflict view resolves.
    QVector<StagedDocument> conflicted() const;
    int conflictCount() const;

    const StagedDocument* documentAtRow(int row) const;
    std::optional<MutationValue> value(int row, const QString& field) const;
    bool isDeleted(int row) const;

    // Stage one typed cell. The last thing typed for a field is what gets
    // written; retyping keeps the loaded value it was FIRST typed over — that
    // is the version the guard was taken against.
    void stage(const QString& id, int row, const QVector<FieldValue>& key,
               const QVector<FieldValue>& expect, const QString& field,
               const MutationValue& value,
               const std::optional<MutationValue>& loaded);

    // Stage a whole document for deletion. Its field edits are kept, not
    // dropped: undoing the delete has to give them back.
    void stageDelete(const QString& id, int row, const QVector<FieldValue>& key,
                     const QVector<FieldValue>& expect);

    // Drop one field's staged value. A document left with nothing staged stops
    // being staged at all, rather than lingering as a no-op write.
    void unstage(int row, const QString& field);

    // Drop everything staged for one row.
    void discardRow(int row);
    void discardAll();

    // Re-guard one document with values from a re-read the user has just been
    // shown — a rebase, not the silent retry the guard exists to prevent. The
    // typed values are untouched. Returns the grid row to repaint, or nullopt
    // when the document is no longer staged.
    std::optional<int> rebase(const QString& id, const QVector<FieldValue>& expect);

    // Drop one whole document's staged edits, by id.
    std::optional<int> discardById(const QString& id);

    // Fold a commit report back into the staging list. Matched BY POSITION:
    // the engine returns one row per submitted mutation, in submission order —
    // matching by id would need this layer to know which identity field IS the
    // id. A report that does not line up one-for-one is not guessed at:
    // nothing is folded in and every document stays pending, the safe reading.
    bool apply(const MutationReport& report, const QStringList& committedIds);

signals:
    // Something was staged, discarded or resolved — the bar redraws from this.
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
