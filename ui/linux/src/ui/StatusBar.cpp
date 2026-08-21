#include "StatusBar.hpp"

#include <QHBoxLayout>
#include <QLabel>
#include <QPushButton>

namespace {

QString formatCount(std::uint64_t n) {
    QString s = QString::number(static_cast<qulonglong>(n));
    int pos = s.size() - 3;
    while (pos > 0) {
        s.insert(pos, QLatin1Char(','));
        pos -= 3;
    }
    return s;
}

QString formatElapsed(std::uint64_t ms) {
    if (ms < 1000) {
        return QStringLiteral("%1 ms").arg(static_cast<qulonglong>(ms));
    }
    return QStringLiteral("%1 s").arg(QString::number(ms / 1000.0, 'f', 2));
}

// A subdued fixed-width chip so state / rows / elapsed line up and never wrap.
QLabel* makeChip(QWidget* parent) {
    auto* l = new QLabel(parent);
    l->setTextInteractionFlags(Qt::NoTextInteraction);
    return l;
}

}  // namespace

StatusBar::StatusBar(QWidget* parent) : QWidget(parent) {
    auto* layout = new QHBoxLayout(this);
    layout->setContentsMargins(8, 2, 8, 2);
    layout->setSpacing(12);

    identityLabel_ = makeChip(this);
    identityLabel_->setTextInteractionFlags(Qt::TextSelectableByMouse);
    identityLabel_->hide();
    stateLabel_ = makeChip(this);
    rowsLabel_ = makeChip(this);
    noticeLabel_ = makeChip(this);
    noticeLabel_->setStyleSheet(QStringLiteral("color: #c0700a; font-weight: 600;"));
    elapsedLabel_ = makeChip(this);
    readOnlyLabel_ = makeChip(this);

    messageLabel_ = new QLabel(this);
    messageLabel_->setTextInteractionFlags(Qt::TextSelectableByMouse);
    messageLabel_->setSizePolicy(QSizePolicy::Expanding, QSizePolicy::Preferred);

    cancelButton_ = new QPushButton(QStringLiteral("Cancel"), this);
    cancelButton_->setEnabled(false);
    cancelButton_->setToolTip(QStringLiteral("Cancel the running statement"));
    connect(cancelButton_, &QPushButton::clicked, this, &StatusBar::cancelRequested);

    layout->addWidget(identityLabel_);
    layout->addWidget(stateLabel_);
    layout->addWidget(rowsLabel_);
    layout->addWidget(noticeLabel_);
    layout->addWidget(elapsedLabel_);
    layout->addWidget(readOnlyLabel_);
    layout->addWidget(messageLabel_, 1);
    layout->addWidget(cancelButton_);
}

void StatusBar::showIdentity(const QString& text, const QString& tooltip) {
    identityLabel_->setText(text);
    identityLabel_->setToolTip(tooltip);
    identityLabel_->setVisible(!text.isEmpty());
}

void StatusBar::setLimitHint(std::optional<std::uint64_t> limit) {
    limitHint_ = limit;
}

void StatusBar::beginQuery() {
    limitHint_.reset();
    noticeLabel_->clear();
    cancelButton_->setEnabled(true);
}

void StatusBar::showMessage(const QString& text, bool error) {
    messageLabel_->setText(text);
    messageLabel_->setStyleSheet(error ? QStringLiteral("color: #c0392b;")
                                       : QString());
}

bool StatusBar::limitHit(const dg::QueryStatus& s) const {
    if (s.state != dg::QueryState::Done || !limitHint_ || *limitHint_ == 0) {
        return false;
    }
    return s.rowsLoaded >= *limitHint_;
}

QString StatusBar::rowCountText(const dg::QueryStatus& s) const {
    // A write reports affected rows, not a fetched-row count.
    if (s.affectedRows.has_value()) {
        return QStringLiteral("%1 affected").arg(formatCount(*s.affectedRows));
    }
    if (s.capped() || limitHit(s)) {
        return QStringLiteral("first %1 rows").arg(formatCount(s.rowsLoaded));
    }
    if (s.streaming()) {
        return QStringLiteral("%1 rows so far…").arg(formatCount(s.rowsLoaded));
    }
    if (!s.totalKnown) {
        return QStringLiteral("≥ %1 rows").arg(formatCount(s.rowsLoaded));
    }
    return QStringLiteral("%1 rows").arg(formatCount(s.rowsLoaded));
}

QString StatusBar::incompleteNotice(const dg::QueryStatus& s) const {
    if (s.capped()) {
        return QStringLiteral("stopped at the %1-row cap — result incomplete")
            .arg(formatCount(s.rowsLoaded));
    }
    if (limitHit(s)) {
        return QStringLiteral("showing first %1 rows (@limit)")
            .arg(formatCount(*limitHint_));
    }
    return QString();
}

QString StatusBar::readOnlyBadge(const dg::QueryStatus& s) {
    if (s.readOnlyEnforcement.isEmpty()) {
        return QString();  // writeable profile — nothing to badge
    }
    if (s.readOnlyEnforcement == QStringLiteral("server")) {
        return s.readOnlyServerConfirmed
                   ? QStringLiteral("read-only (server)")
                   : QStringLiteral("read-only (server, unconfirmed)");
    }
    if (s.readOnlyEnforcement == QStringLiteral("client")) {
        return QStringLiteral("read-only (datagrep only)");
    }
    // "none": datagrep refuses writes, but nothing on the server does.
    return QStringLiteral("read-only (datagrep only — server unguarded)");
}

void StatusBar::updateStatus(const dg::QueryStatus& status) {
    QString stateText;
    switch (status.state) {
        case dg::QueryState::Streaming: stateText = QStringLiteral("streaming"); break;
        case dg::QueryState::Parked: stateText = QStringLiteral("parked"); break;
        case dg::QueryState::Capped: stateText = QStringLiteral("capped"); break;
        case dg::QueryState::Done: stateText = QStringLiteral("done"); break;
        case dg::QueryState::Cancelled: stateText = QStringLiteral("cancelled"); break;
        case dg::QueryState::Failed: stateText = QStringLiteral("failed"); break;
    }
    stateLabel_->setText(stateText);

    rowsLabel_->setText(rowCountText(status));
    rowsLabel_->setToolTip(
        status.capped()
            ? QStringLiteral("the engine stopped storing rows at its cap — this "
                             "is not the whole result")
            : (!status.totalKnown && status.streaming() == false
                   ? QStringLiteral("≥ because this engine streams without "
                                    "reporting a total — more rows may exist")
                   : QString()));

    const QString notice = incompleteNotice(status);
    noticeLabel_->setText(notice);
    noticeLabel_->setToolTip(
        status.capped()
            ? QStringLiteral("the engine's soft row cap ended this result early; "
                             "rows beyond this point exist but were not fetched — "
                             "narrow the query to see them")
            : (notice.isEmpty()
                   ? QString()
                   : QStringLiteral("an @limit directive stopped this result "
                                    "early — the full result may be longer; "
                                    "raise or remove the @limit to fetch more")));

    elapsedLabel_->setText(formatElapsed(status.elapsedMs));

    readOnlyLabel_->setText(readOnlyBadge(status));

    if (!status.error.isEmpty()) {
        showMessage(status.error, /*error=*/true);
    }

    // Cancel is live only while there is genuinely something to cancel.
    cancelButton_->setEnabled(status.streaming());
}
