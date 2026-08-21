// Deliberately never offers "retry as written" — that is the clobber the guard exists to prevent.
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

    bool movedUnderneath() const;
};

struct ConflictDocument {
    QString id;     // the staged document's own id — how a resolution finds it
    QString title;  // the identity, spelled the way the engine spells it
    QVector<ConflictField> fields;
    bool isDelete = false;
    QVector<FieldValue> rebaseGuard;
    bool canRebase = false;
    bool gone = false;  // no longer on the server
    QString error;      // why this one could not be read, when it could not

    int contestedCount() const;
};

// Every conflicted document from one commit, with what the server holds now.
struct ConflictReview {
    QVector<ConflictDocument> documents;

    static ConflictReview build(const QVector<StagedDocument>& conflicted,
                                const QVector<ServerDocument>& server,
                                const EditableResult& editable);
};

}  // namespace dg

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
