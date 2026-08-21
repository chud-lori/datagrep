// StatusBar.hpp — the honest results status bar.
//
// The Linux analogue of the macOS StatusBar (ui/macos/.../StatusBar.swift). It
// shows connection state, an HONEST row count, elapsed time, the read-only
// enforcement badge, a free-text message, and a Cancel button — nothing here
// animates and nothing polls: every value changes only when the model hands it
// a fresh dg::QueryStatus (fed by datagrep_query_status_json) or the user acts.
//
// The one rule this widget exists to enforce is the lesson the macOS bar
// spells out, learned from clients that get it wrong: a partial result must
// never print a row count that looks final. So:
//
//   * a server-capped result says "first N rows", not "N rows";
//   * an @limit-truncated result says "first N rows (@limit)";
//   * an engine that streams without a known total says "≥ N rows";
//   * only a genuinely complete result says the bare "N rows".
//
// It holds NO business logic and never touches the C ABI: it is handed a decoded
// dg::QueryStatus and (optionally) the @limit the caller parsed from the block
// under the caret, and it renders. Cancel is a signal the MainWindow wires to
// the model's cancel(), which is the one path allowed to call
// datagrep_query_cancel and free its outcome JSON.

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

    // Re-render everything from a fresh status snapshot. Called on every
    // ResultModel::statusChanged tick (which the model marshals onto the GUI
    // thread for us).
    void updateStatus(const dg::QueryStatus& status);

    // The @limit N a caller parsed from the statement being run, or nullopt when
    // the statement carried no @limit. Used ONLY to decide whether a completed
    // result is "first N rows (@limit)" rather than a final "N rows"; it never
    // manufactures a count. Set it before the query's first status tick.
    void setLimitHint(std::optional<std::uint64_t> limit);

    // A new query is starting: clear the per-result honesty state (the previous
    // @limit, any lingering notice) and re-enable Cancel. Message is left alone
    // so a "select a connection first" note is not wiped by a run that never
    // started.
    void beginQuery();

    // Free-text status line (errors, hints, copy confirmations). `error` tints it.
    void showMessage(const QString& text, bool error = false);

signals:
    // The Cancel button was pressed. MainWindow routes this to the model, which
    // owns the datagrep_query_cancel call and frees the outcome JSON exactly once.
    void cancelRequested();

private:
    // The honest count string for the current status + limit hint.
    QString rowCountText(const dg::QueryStatus& s) const;
    // The orange one-liner shown when the result is provably incomplete, or empty.
    QString incompleteNotice(const dg::QueryStatus& s) const;
    // The read-only badge text, worded down so a client-only guard is never
    // mistaken for the server refusing writes. Empty for a writeable profile.
    static QString readOnlyBadge(const dg::QueryStatus& s);
    // True when a completed result sat exactly at (or past) the @limit — the
    // grid holds the first N of a possibly longer result.
    bool limitHit(const dg::QueryStatus& s) const;

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
