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

    // datagrep_mutate blocks, so the commit runs off-thread and reports back queued.
    void sendMutations(const QVector<dg::StagedDocument>& pending,
                       const QString& profile);
    void finishCommit(const QString& reportJson, const QString& failure,
                      const QStringList& ids, const QVector<int>& rows);
    void finishReread(const QString& serverJson, const QString& failure,
                      const QVector<dg::StagedDocument>& conflicted);
    void presentReport(const dg::MutationReport& report);
    void presentConflictReview();

    // The sentence that has to be read before the click; numbered, not abstract.
    static QString commitWarning(int count, bool atomic);
    static QString reportHeadline(const dg::MutationReport& report);

    // The one run path — the confirm-writes prompt and history record cannot be bypassed.
    void executeStatement(const QString& profile, const QString& sql);
    bool selectConnection(const QString& name);

    // Marker band for the selected connection; refreshed on selection and reload.
    void updateMarkedBanner();

    // Identity chip from datagrep_connection_info_json; off-thread, stale answers dropped.
    void refreshConnectionInfo();
    void applyConnectionInfo(const QString& profile, const QString& json);

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

    // What the current result ran against, whatever the sidebar selected since.
    QString lastProfile_;
    QString lastSql_;
    dg::ConflictReview conflictReview_;
    bool isCommitting_ = false;
    bool isRereading_ = false;

    bool infoInFlight_ = false;
    bool infoRefreshedForQuery_ = false;
    QString infoPending_;
    QString infoShownProfile_;

    // Safety facts per profile; the list rows, banner and run path all read this map.
    QHash<QString, dg::ConnectionSafety> safetyByProfile_;
    // Driver id per profile, for the engine field history stores on each entry.
    QHash<QString, QString> driverByProfile_;
};

#endif  // DATAGREP_MAIN_WINDOW_HPP
