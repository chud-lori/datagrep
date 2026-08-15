// MainWindow.hpp — the datagrep Linux workbench window.
//
// Sidebar (connections + lazy schema tree) | SQL editor over a virtualised
// results grid, with an honest status bar (rows / elapsed / capped / read-only /
// cancel). Every non-trivial behaviour delegates straight to the C ABI through
// the dg:: wrappers; this window holds no business logic.

#ifndef DATAGREP_MAIN_WINDOW_HPP
#define DATAGREP_MAIN_WINDOW_HPP

#include "model/QueryStatus.hpp"

#include <QMainWindow>

#include <memory>

namespace dg {
class Core;
}
class ResultModel;
class ResultTableView;
class SqlEditor;
class SchemaTree;
class QListWidget;
class QLabel;
class QPushButton;

class MainWindow : public QMainWindow {
    Q_OBJECT

public:
    explicit MainWindow(QWidget* parent = nullptr);
    ~MainWindow() override;

private slots:
    void reloadProfiles();
    void onConnectionSelected();
    void runStatement();
    void cancelQuery();
    void onStatusChanged(const dg::QueryStatus& status);
    void onSchemaObjectActivated(const QString& profile, const QString& pathJson);

private:
    QString selectedProfile() const;

    std::unique_ptr<dg::Core> core_;

    QListWidget* connections_;
    SchemaTree* schema_;
    SqlEditor* editor_;
    ResultTableView* grid_;
    ResultModel* model_;

    // Status bar widgets.
    QLabel* rowsLabel_;
    QLabel* elapsedLabel_;
    QLabel* stateLabel_;
    QLabel* readOnlyLabel_;
    QLabel* messageLabel_;
    QPushButton* cancelButton_;
};

#endif  // DATAGREP_MAIN_WINDOW_HPP
