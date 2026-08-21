#include "RowNumberHeader.hpp"

#include <QFontMetrics>

#include <algorithm>

RowNumberHeader::RowNumberHeader(QWidget* parent)
    : QHeaderView(Qt::Vertical, parent) {
    setSectionResizeMode(QHeaderView::Fixed);
    setSectionsClickable(true);   // click a number to select the whole row
    setSectionsMovable(false);
    setHighlightSections(true);
    setDefaultAlignment(Qt::AlignRight | Qt::AlignVCenter);
}

void RowNumberHeader::updateWidthForRowCount(int rowCount) {
    const int d = std::max(2, static_cast<int>(
                                  QString::number(std::max(rowCount, 1)).size()));
    if (d != digits_) {
        digits_ = d;
        // Ask the layout to re-read sizeHint() so the new width takes effect.
        updateGeometry();
    }
}

QSize RowNumberHeader::sizeHint() const {
    QSize base = QHeaderView::sizeHint();
    const QFontMetrics fm(font());
    const int textWidth = fm.horizontalAdvance(QString(digits_, QLatin1Char('0')));
    const int padding = 18;  // leading + trailing chrome padding
    base.setWidth(textWidth + padding);
    return base;
}
