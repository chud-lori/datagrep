// UpdateNotice.hpp — the visible half of UpdateCheck: an inline bar that
// exists only while a newer release is known. It renders nothing otherwise,
// animates nothing, and never installs — "View release" opens the browser.

#ifndef DATAGREP_UPDATE_NOTICE_HPP
#define DATAGREP_UPDATE_NOTICE_HPP

#include "model/UpdateCheck.hpp"

#include <QWidget>

class QLabel;

class UpdateNotice : public QWidget {
    Q_OBJECT

public:
    explicit UpdateNotice(UpdateCheck* check, QWidget* parent = nullptr);

private slots:
    void showManifest(const dg::UpdateManifest& manifest);

private:
    void openRelease() const;

    UpdateCheck* check_;
    dg::UpdateManifest manifest_;
    QLabel* label_;
};

#endif  // DATAGREP_UPDATE_NOTICE_HPP
