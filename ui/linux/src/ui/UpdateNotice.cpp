#include "UpdateNotice.hpp"

#include <QDesktopServices>
#include <QHBoxLayout>
#include <QLabel>
#include <QMenu>
#include <QPushButton>
#include <QToolButton>

UpdateNotice::UpdateNotice(UpdateCheck* check, QWidget* parent)
    : QWidget(parent), check_(check), label_(new QLabel(this)) {
    // Palette roles, not hard-coded colours, so the bar follows every
    // appearance mode.
    setAutoFillBackground(true);
    setBackgroundRole(QPalette::AlternateBase);

    auto* view = new QPushButton(QStringLiteral("View release"), this);
    view->setFlat(true);
    view->setCursor(Qt::PointingHandCursor);
    connect(view, &QPushButton::clicked, this, &UpdateNotice::openRelease);

    auto* more = new QToolButton(this);
    more->setText(QStringLiteral("⋯"));
    more->setAutoRaise(true);
    more->setPopupMode(QToolButton::InstantPopup);
    more->setToolTip(
        QStringLiteral("Skip this version, or turn update checks off"));
    auto* menu = new QMenu(more);
    menu->addAction(QStringLiteral("Skip this version"), this, [this]() {
        check_->skip(manifest_);
        hide();
    });
    menu->addAction(QStringLiteral("Turn off update checks"), this, [this]() {
        UpdateCheck::setCheckOnLaunchEnabled(false);
        hide();
    });
    more->setMenu(menu);

    auto* dismiss = new QToolButton(this);
    dismiss->setText(QStringLiteral("×"));
    dismiss->setAutoRaise(true);
    dismiss->setToolTip(QStringLiteral("Dismiss (until the next launch)"));
    connect(dismiss, &QToolButton::clicked, this, &QWidget::hide);

    auto* layout = new QHBoxLayout(this);
    layout->setContentsMargins(10, 4, 6, 4);
    layout->setSpacing(6);
    layout->addWidget(label_);
    layout->addWidget(view);
    layout->addStretch(1);
    layout->addWidget(more);
    layout->addWidget(dismiss);

    hide();
    connect(check_, &UpdateCheck::updateAvailable, this,
            &UpdateNotice::showManifest);
}

void UpdateNotice::showManifest(const dg::UpdateManifest& manifest) {
    manifest_ = manifest;
    label_->setText(QStringLiteral("datagrep %1 is available")
                        .arg(UpdateCheck::normalize(manifest.version)));
    show();
}

void UpdateNotice::openRelease() const {
    const QUrl url =
        manifest_.releaseUrl.isValid() && !manifest_.releaseUrl.isEmpty()
            ? manifest_.releaseUrl
            : QUrl(QStringLiteral("https://github.com/chud-lori/datagrep/releases"));
    QDesktopServices::openUrl(url);
}
