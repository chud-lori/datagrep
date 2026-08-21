// DetailPanel.hpp — the inspector: schema and cell detail, two tabs.

#ifndef DATAGREP_DETAIL_PANEL_HPP
#define DATAGREP_DETAIL_PANEL_HPP

#include <QTabWidget>

class QLabel;
class QPlainTextEdit;
class QPushButton;
class QTreeWidget;

class DetailPanel : public QTabWidget {
    Q_OBJECT

public:
    explicit DetailPanel(QWidget* parent = nullptr);

public slots:
    // Draw one describe() payload (or its failure / nothing-selected state).
    void showSchema(const QString& profile, const QString& pathJson,
                    const QString& describeJson, const QString& error);

    void showCell(int row, int column, const QString& detailJson,
                  const QString& envelopeJson, bool raise);

    // A new query invalidates every row/column the cell pane could be naming.
    void clearCell();

signals:
    void cellCopied();

private:
    void buildSchemaTab();
    void buildCellTab();

    // Schema tab.
    QLabel* schemaTitle_;
    QLabel* schemaSubtitle_;
    QLabel* schemaStats_;
    QTreeWidget* schemaTree_;

    // Cell tab.
    QLabel* cellTitle_;
    QPushButton* cellCopyButton_;
    QPlainTextEdit* cellText_;
};

#endif  // DATAGREP_DETAIL_PANEL_HPP
