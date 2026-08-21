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

#include <QHash>
#include <QMainWindow>

#include <memory>

namespace dg {
class Core;
}
class ResultModel;
class ResultTableView;
class SqlEditor;
class SchemaTree;
class StatusBar;
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

private:
    QString selectedProfile() const;

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
    SqlEditor* editor_;
    ResultTableView* grid_;
    ResultModel* model_;
    StatusBar* status_;

    // The safety facts per profile, rebuilt on every reloadProfiles(). The list
    // rows, the banner and the run path all read THIS map, so they can never
    // disagree about how careful a connection wants us to be.
    QHash<QString, dg::ConnectionSafety> safetyByProfile_;
};

#endif  // DATAGREP_MAIN_WINDOW_HPP
