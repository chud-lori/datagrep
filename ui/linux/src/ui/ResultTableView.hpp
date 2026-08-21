// ResultTableView.hpp — the QTableView bound to ResultModel.

#ifndef DATAGREP_RESULT_TABLE_VIEW_HPP
#define DATAGREP_RESULT_TABLE_VIEW_HPP

#include <QTableView>

class QContextMenuEvent;
class ResultModel;
class RowNumberHeader;

class ResultTableView : public QTableView {
    Q_OBJECT

public:
    explicit ResultTableView(QWidget* parent = nullptr);

    void setModel(QAbstractItemModel* model) override;

public slots:
    // Copies the current selection as TSV. Bound to QKeySequence::Copy.
    // Reads selectedIndexes() only, so header row numbers structurally cannot appear.
    void copySelection() const;

protected:
    void contextMenuEvent(QContextMenuEvent* event) override;

private:
    RowNumberHeader* rowHeader_;
};

#endif  // DATAGREP_RESULT_TABLE_VIEW_HPP
