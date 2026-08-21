// MainWindow.hpp — the datagrep Linux workbench window.
//
// Sidebar (connections + lazy schema tree) | SQL editor over a virtualised
// results grid, with an honest status bar (rows / elapsed / capped / read-only /
// cancel). Every non-trivial behaviour delegates straight to the C ABI through
// the dg:: wrappers; this window holds no business logic.

#ifndef DATAGREP_MAIN_WINDOW_HPP
#define DATAGREP_MAIN_WINDOW_HPP

#include "model/ConnectionSafety.hpp"
#include "model/QueryStatus.hpp"
#include "ui/ConflictResolution.hpp"

#include <QHash>
#include <QMainWindow>

#include <memory>

namespace dg {
class Core;
class PendingEdits;
}
class DetailPanel;
class EditorTabs;
class HistoryPanel;
class QueryHistoryStore;
class ResultModel;
class ResultTableView;
class SchemaTree;
class StagedEditsBar;
class StatusBar;
class UpdateCheck;
class UpdateNotice;
class QDockWidget;
class QLabel;
class QListWidget;
class QPushButton;

class MainWindow : public QMainWindow {
    Q_OBJECT

public:
    explicit MainWindow(QWidget* parent = nullptr);
    ~MainWindow() override;

private slots:
    void reloadProfiles();
    void onConnectionSelected();
    void onAddConnection();
    void onEditConnection();
    void onRemoveConnection();
    void runStatement();
    void cancelQuery();
    void onStatusChanged(const dg::QueryStatus& status);
    void onSchemaObjectActivated(const QString& profile, const QString& pathJson);
    void onOpenHistoryInEditor(const QString& sql, const QString& connection);
    void onRerunFromHistory(const QString& sql, const QString& connection);

    // --- staged document edits (Elasticsearch grid editing) ----------------
    void commitStagedEdits();
    void discardStagedEditsPrompt();
    void reviewConflicts();
    void reloadResult();

private:
    QString selectedProfile() const;

    // The commit itself, after the confirmation. datagrep_mutate blocks, so it
    // runs on its own thread and reports back through a queued call — the
    // window keeps drawing and says "committing…" instead of freezing on a
    // cluster that is thinking.
    void sendMutations(const QVector<dg::StagedDocument>& pending,
                       const QString& profile);
    void finishCommit(const QString& reportJson, const QString& failure,
                      const QStringList& ids, const QVector<int>& rows);
    void finishReread(const QString& serverJson, const QString& failure,
                      const QVector<dg::StagedDocument>& conflicted);
    void presentReport(const dg::MutationReport& report);
    void presentConflictReview();

    // The sentence that has to be read before the click. Numbered rather than
    // abstract: "if #3 fails, #1 and #2 stay written" is something someone can
    // picture, where "the batch is not atomic" is something they can nod at.
    static QString commitWarning(int count, bool atomic);
    static QString reportHeadline(const dg::MutationReport& report);

    // The one run path. Every statement — typed or replayed from history —
    // goes through here, so the confirm-writes prompt and the history record
    // can never be bypassed by where the SQL came from.
    void executeStatement(const QString& profile, const QString& sql);
    bool selectConnection(const QString& name);

    // Show the filled marker band for the selected connection, or hide it. Runs
    // on every selection change and after every profile reload, so the banner
    // can never describe a connection other than the one queries would hit.
    void updateMarkedBanner();

    std::unique_ptr<dg::Core> core_;

    QListWidget* connections_;
    QPushButton* addButton_;
    QPushButton* editButton_;
    QPushButton* removeButton_;
    SchemaTree* schema_;
    QLabel* markedBanner_;
    DetailPanel* inspector_;
    QDockWidget* inspectorDock_;
    QueryHistoryStore* history_;
    HistoryPanel* historyPanel_;
    QDockWidget* historyDock_;
    EditorTabs* editors_;
    ResultTableView* grid_;
    ResultModel* model_;
    StatusBar* status_;
    dg::PendingEdits* edits_;
    StagedEditsBar* stagedBar_;
    UpdateCheck* updateCheck_;
    UpdateNotice* updateNotice_;

    // What the current result was run against — the profile a commit or a
    // re-read must address, whatever the sidebar has selected since.
    QString lastProfile_;
    QString lastSql_;
    dg::ConflictReview conflictReview_;
    bool isCommitting_ = false;
    bool isRereading_ = false;

    // The safety facts per profile, rebuilt on every reloadProfiles(). The list
    // rows, the banner and the run path all read THIS map, so they can never
    // disagree about how careful a connection wants us to be.
    QHash<QString, dg::ConnectionSafety> safetyByProfile_;
    // Driver id per profile, for the engine field history stores on each entry.
    QHash<QString, QString> driverByProfile_;
};

#endif  // DATAGREP_MAIN_WINDOW_HPP
