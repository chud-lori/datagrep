// ConflictResolution.hpp — a version conflict as three readings of the same
// document.
//
// Linux counterpart of the macOS ConflictResolution. A 409 is the guard doing
// its job, and the only useful thing to say about it is WHAT CHANGED: the
// document is read back and put next to what was loaded and what was typed —
// three columns, one row per edited field — and the two honest offers are made
// from there: re-apply my edits onto this version (rebase), or drop them
// (discard mine). Deliberately not offered anywhere: retrying the write as it
// stood. That is the clobber the guard exists to prevent, and it is not any
// less of one for being one click away.

#ifndef DATAGREP_CONFLICT_RESOLUTION_HPP
#define DATAGREP_CONFLICT_RESOLUTION_HPP

#include "model/GridEditing.hpp"
#include "model/Mutation.hpp"

#include <QDialog>
#include <QHash>
#include <QString>
#include <QVector>

class QVBoxLayout;

namespace dg {

// One edited field's three readings.
struct ConflictField {
    QString name;
    std::optional<MutationValue> loaded;  // what the cell held when typed into
    ServerValue server;                   // what the server holds now
    MutationValue typed;                  // what a rebase would write

    // The server moved this field, not just the document around it — a rebase
    // overwrites somebody else's value here, and the row says so.
    bool movedUnderneath() const;
};

// One conflicted document, ready to review. Every value computed when the
// re-read landed: the dialog does no lookups, so nothing in a button can throw.
struct ConflictDocument {
    QString id;     // the staged document's own id — how a resolution finds it
    QString title;  // the identity, spelled the way the engine spells it
    QVector<ConflictField> fields;
    bool isDelete = false;
    // The guard values a rebase would re-guard against; empty when the re-read
    // did not bring back a usable one — rebase is then not offered rather than
    // sent unguarded.
    QVector<FieldValue> rebaseGuard;
    bool canRebase = false;
    bool gone = false;  // no longer on the server
    QString error;      // why this one could not be read, when it could not

    int contestedCount() const;
};

// Every conflicted document from one commit, with what the server holds now.
// Built from staged documents and the re-read that answers them — matched BY
// POSITION, the contract datagrep_reread_documents states. The caller checks
// the two counts agree before getting here.
struct ConflictReview {
    QVector<ConflictDocument> documents;

    static ConflictReview build(const QVector<StagedDocument>& conflicted,
                                const QVector<ServerDocument>& server,
                                const EditableResult& editable);
};

}  // namespace dg

// The three-column conflict dialog. Emits which resolution was chosen for
// which document; MainWindow applies it to the staging store and calls
// removeDocument, which closes the dialog once nothing is left to read.
class ConflictReviewDialog : public QDialog {
    Q_OBJECT

public:
    ConflictReviewDialog(const dg::ConflictReview& review, QWidget* parent = nullptr);

public slots:
    void removeDocument(const QString& id);

signals:
    void rebaseChosen(const QString& id);
    void discardChosen(const QString& id);

private:
    QWidget* buildDocumentBlock(const dg::ConflictDocument& document);

    QVBoxLayout* listLayout_;
    QHash<QString, QWidget*> blocks_;
};

#endif  // DATAGREP_CONFLICT_RESOLUTION_HPP
