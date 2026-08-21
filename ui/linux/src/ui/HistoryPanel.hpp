// HistoryPanel.hpp — the query history dock: everything datagrep ran, searchable.
//
// Linux counterpart of the macOS HistoryPanel, in Qt vocabulary: a widget for a
// bottom QDockWidget rather than a sheet. Same functionality — filter by text,
// connection, date and outcome; entries grouped by day; a detail strip with the
// full statement and its error; copy / open in editor / run again / remove;
// retention stated in the footer and editable. The panel never runs anything
// itself: it emits requests and MainWindow owns the run path, so a rerun goes
// through exactly the same safety prompts as a typed statement.

#ifndef DATAGREP_HISTORY_PANEL_HPP
#define DATAGREP_HISTORY_PANEL_HPP

#include "model/QueryHistory.hpp"

#include <QWidget>

class QComboBox;
class QLabel;
class QLineEdit;
class QPlainTextEdit;
class QPushButton;
class QToolButton;
class QTreeWidget;

class HistoryPanel : public QWidget {
    Q_OBJECT

public:
    explicit HistoryPanel(QueryHistoryStore* store, QWidget* parent = nullptr);

signals:
    // "Put this SQL where I can edit it." The host decides how (append today,
    // a new tab once tabs exist) — the panel only asks.
    void openInEditor(const QString& sql, const QString& connection);
    void rerunRequested(const QString& sql, const QString& connection);
    // Short confirmations for the status bar, where this app says small things.
    void statusMessage(const QString& text);

private slots:
    void refresh();
    void onSelectionChanged();

private:
    dg::HistoryFilter currentFilter() const;
    std::optional<dg::QueryHistoryEntry> selectedEntry() const;
    void rebuildConnectionCombo(const QStringList& names);
    void showDetail(const std::optional<dg::QueryHistoryEntry>& entry);
    void copySelected();
    void openSelected();
    void rerunSelected();
    void removeSelected();
    void editRetention();

    QueryHistoryStore* store_;

    QLineEdit* search_;
    QComboBox* connectionFilter_;
    QComboBox* rangeFilter_;
    QComboBox* outcomeFilter_;
    QPushButton* clearFiltersButton_;
    QLabel* countLabel_;

    QTreeWidget* list_;

    QWidget* detail_;
    QLabel* detailSummary_;
    QPlainTextEdit* detailSql_;
    QPlainTextEdit* detailError_;

    QLabel* retentionLabel_;
    QPushButton* retentionButton_;
    QToolButton* clearButton_;

    QString selectedId_;
    bool refreshing_ = false;
};

#endif  // DATAGREP_HISTORY_PANEL_HPP
