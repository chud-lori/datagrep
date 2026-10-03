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
    // Reads selectedIndexes() only, so header row numbers cannot reach the clipboard.
    void copySelection() const;

protected:
    void contextMenuEvent(QContextMenuEvent* event) override;

private:
    RowNumberHeader* rowHeader_;
};

#endif  // DATAGREP_RESULT_TABLE_VIEW_HPP
