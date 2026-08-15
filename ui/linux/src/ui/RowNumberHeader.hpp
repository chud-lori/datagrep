// RowNumberHeader.hpp — the vertical (row-number) header of the results grid.
//
// This is the Linux analogue of the macOS GridRowNumberRuler (an NSRulerView).
// The choice of a QHeaderView is load-bearing exactly as NSRulerView was:
//
//  1. PINNED BY CONSTRUCTION — a QTableView's vertical header lives outside the
//     horizontally-scrolling viewport, so horizontal scroll can never move it.
//
//  2. EXCLUDED FROM COPY BY CONSTRUCTION — the row number is produced ONLY by
//     ResultModel::headerData(section, Qt::Vertical). A header value is not a
//     QModelIndex and is therefore never a member of
//     selectionModel()->selectedIndexes(). Every copy path in ResultTableView
//     serialises selectedIndexes() and nothing else, so the row number is
//     STRUCTURALLY incapable of reaching the clipboard. It is chrome, not data.
//     (Compare the macOS guarantee: the number lived in an NSRulerView, and the
//     copy paths enumerate tableColumns only.)
//
//  3. CANNOT BREAK VIRTUALISATION — the number is section+1, a pure function of
//     the row index. Painting it never touches the RowPager, so scrolling the
//     gutter costs zero row fetches and zero page-cache churn.
//
// This subclass only adapts the gutter WIDTH to the magnitude of the row count
// (1,000,000 rows must not clip to "1000…") and keeps every section a fixed
// height so geometry stays arithmetic (no resizeRowsToContents ever). None of
// that affects the copy-safety guarantee, which is a property of the header
// being a header.

#ifndef DATAGREP_ROW_NUMBER_HEADER_HPP
#define DATAGREP_ROW_NUMBER_HEADER_HPP

#include <QHeaderView>

class RowNumberHeader : public QHeaderView {
    Q_OBJECT

public:
    explicit RowNumberHeader(QWidget* parent = nullptr);

    // Recompute the gutter thickness from the largest row number the grid can
    // show (1,000,000 rows must not clip to "1000…").
    void updateWidthForRowCount(int rowCount);

    // The gutter thickness. For a vertical header the widget's width comes from
    // sizeHint().width(); the section resize mode (Fixed) governs only the row
    // height along the vertical axis, so width is set here, not via
    // sectionSizeFromContents.
    QSize sizeHint() const override;

private:
    int digits_ = 2;  // min two digits so a small result still reads as a column
};

#endif  // DATAGREP_ROW_NUMBER_HEADER_HPP
