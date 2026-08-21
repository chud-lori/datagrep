// StatusBar.hpp — the honest results status bar.

#ifndef DATAGREP_STATUS_BAR_HPP
#define DATAGREP_STATUS_BAR_HPP

#include "model/QueryStatus.hpp"

#include <QWidget>

#include <cstdint>
#include <optional>

class QLabel;
class QPushButton;

class StatusBar : public QWidget {
    Q_OBJECT

public:
    explicit StatusBar(QWidget* parent = nullptr);

    // Re-render from a fresh snapshot (every ResultModel::statusChanged tick).
    void updateStatus(const dg::QueryStatus& status);

    void setLimitHint(std::optional<std::uint64_t> limit);

    void beginQuery();

    // Free-text status line (errors, hints, copy confirmations). `error` tints it.
    void showMessage(const QString& text, bool error = false);

    // Where the selected connection points: "profile · product version · db".
    // Empty text hides the chip.
    void showIdentity(const QString& text, const QString& tooltip);

signals:
    void cancelRequested();

private:
    // A partial result must never print a count that looks final.
    QString rowCountText(const dg::QueryStatus& s) const;
    QString incompleteNotice(const dg::QueryStatus& s) const;
    static QString readOnlyBadge(const dg::QueryStatus& s);
    // True when a completed result sat at/past the @limit.
    bool limitHit(const dg::QueryStatus& s) const;

    QLabel* identityLabel_;
    QLabel* stateLabel_;
    QLabel* rowsLabel_;
    QLabel* noticeLabel_;   // incomplete-result warning, orange
    QLabel* elapsedLabel_;
    QLabel* readOnlyLabel_;
    QLabel* messageLabel_;
    QPushButton* cancelButton_;

    std::optional<std::uint64_t> limitHint_;
};

#endif  // DATAGREP_STATUS_BAR_HPP
