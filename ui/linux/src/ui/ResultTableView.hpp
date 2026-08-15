// ResultTableView.hpp — the QTableView bound to ResultModel, with a copy path
// that is provably free of row numbers.
//
// Qt's item views ship NO built-in Ctrl+C, so copy is implemented here. The
// implementation reads selectionModel()->selectedIndexes() — model cells only —
// and joins them with tabs and newlines. Because the row-number gutter is a
// QHeaderView (see RowNumberHeader.hpp) and header values are never QModelIndex,
// they cannot appear in selectedIndexes(), and therefore cannot appear in copied
// output. This is the structural copy-safety guarantee, mirroring the macOS
// grid whose copy paths enumerate tableColumns only.

#ifndef DATAGREP_RESULT_TABLE_VIEW_HPP
#define DATAGREP_RESULT_TABLE_VIEW_HPP

#include <QTableView>

class RowNumberHeader;

class ResultTableView : public QTableView {
    Q_OBJECT

public:
    explicit ResultTableView(QWidget* parent = nullptr);

    void setModel(QAbstractItemModel* model) override;

public slots:
    // Copies the current selection to the clipboard as TSV (tab between columns,
    // newline between rows). Row numbers are structurally excluded — see the file
    // header. Bound to QKeySequence::Copy.
    void copySelection() const;

private:
    RowNumberHeader* rowHeader_;
};

#endif  // DATAGREP_RESULT_TABLE_VIEW_HPP
