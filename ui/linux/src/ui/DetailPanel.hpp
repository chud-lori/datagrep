// DetailPanel.hpp — the inspector: schema and cell detail, two tabs.
//
// This is the Linux counterpart of the macOS DetailPanel (SchemaPane +
// CellDetailPane behind one mode switch). The same two questions, the same
// honesty rules, in Qt vocabulary — a QTabWidget inside a QDockWidget rather
// than a segmented picker over a material:
//
//  * Schema — what the selected table/collection/key *is*: columns, indexes,
//    stats. Fed by SchemaTree::objectDescribed (one datagrep_catalog_describe
//    call per object, made by the tree; this panel never describes anything
//    itself). The cardinal rule is inherited from the describe contract: draw
//    what arrived, say plainly what did not, and never imply a fact the engine
//    did not report. `[]` and `null` for indexes are two different sentences —
//    "no indexes" versus "indexes not reported" — because they are two
//    different facts.
//
//  * Cell — what one value *contains*. Clicking a cell shows its full raw JSON
//    (datagrep_rows_cell_detail_json) here, pretty-printed; a nested cell —
//    the `{n fields}` chip — additionally raises this tab, because clicking a
//    chip is an unambiguous request to see inside it. When the row carries an
//    envelope (fields outside the projected root, e.g. which document a value
//    belongs to), the envelope leads, exactly as on macOS: it is the answer to
//    the question that brings someone to this pane.
//
// The tabs are non-destructive on purpose: each keeps its own state, so
// flipping to the schema and back does not throw away the cell being read, and
// switching never re-issues a load.

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
    // Draw one describe() payload (or its failure, or the nothing-selected
    // state). Wired straight to SchemaTree::objectDescribed.
    void showSchema(const QString& profile, const QString& pathJson,
                    const QString& describeJson, const QString& error);

    // Show one cell's full value. `raise` additionally switches to the Cell tab
    // (used for nested chips — an unambiguous request; a plain value click
    // updates the pane without yanking the tab from under the user).
    void showCell(int row, int column, const QString& detailJson,
                  const QString& envelopeJson, bool raise);

    // A new query invalidates every row/column the cell pane could be naming.
    void clearCell();

signals:
    // The copy button was used; MainWindow surfaces the confirmation in the
    // status bar, which is where this app says small things.
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
