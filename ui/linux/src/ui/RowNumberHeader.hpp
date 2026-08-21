// RowNumberHeader.hpp — vertical (row-number) header of the results grid.

#ifndef DATAGREP_ROW_NUMBER_HEADER_HPP
#define DATAGREP_ROW_NUMBER_HEADER_HPP

#include <QHeaderView>

// Row numbers live in headerData(), never selectedIndexes(), so they cannot reach the clipboard.
class RowNumberHeader : public QHeaderView {
    Q_OBJECT

public:
    explicit RowNumberHeader(QWidget* parent = nullptr);

    // Recompute the gutter width from the largest row number the grid can show.
    void updateWidthForRowCount(int rowCount);

    QSize sizeHint() const override;

private:
    int digits_ = 2;  // min two digits so a small result still reads as a column
};

#endif  // DATAGREP_ROW_NUMBER_HEADER_HPP
