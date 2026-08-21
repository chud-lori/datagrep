// EditingSurface.hpp — the bar under the grid while edits are staged, and the
// per-document commit report.
//
// Linux counterpart of the macOS EditingSurface. The bar exists so that "I
// typed something" and "I wrote something" are visibly different states:
// nothing typed into the grid reaches the server until the commit button on
// this bar, and the bar is the standing reminder of unwritten work. The report
// is a dialog, not a toast: a halted batch leaves three kinds of row —
// written, refused, never tried — and a fading message cannot carry that.

#ifndef DATAGREP_EDITING_SURFACE_HPP
#define DATAGREP_EDITING_SURFACE_HPP

#include "model/Mutation.hpp"

#include <QDialog>
#include <QWidget>

class QLabel;
class QPushButton;

namespace dg {
class PendingEdits;
}

class StagedEditsBar : public QWidget {
    Q_OBJECT

public:
    explicit StagedEditsBar(dg::PendingEdits* edits, QWidget* parent = nullptr);

    void setCommitting(bool committing);
    void setRereading(bool rereading);

public slots:
    // Redraws counts and visibility. Connected to PendingEdits::stagingChanged;
    // the bar hides itself when nothing is staged.
    void refresh();

signals:
    void commitRequested();
    void discardRequested();
    void resolveRequested();
    void reloadRequested();

private:
    dg::PendingEdits* edits_;
    QLabel* headline_;
    QLabel* detail_;
    QPushButton* discardButton_;
    QPushButton* resolveButton_;
    QPushButton* commitButton_;
    QPushButton* reloadButton_;
    bool committing_ = false;
    bool rereading_ = false;
};

class MutationReportDialog : public QDialog {
    Q_OBJECT

public:
    MutationReportDialog(const dg::MutationReport& report, QWidget* parent = nullptr);

signals:
    // The only offer a conflict gets here is to go and look at it. What is
    // deliberately absent is a "retry": re-sending the same write against a
    // document that moved is the clobber the guard refused.
    void resolveConflictsRequested();
};

#endif  // DATAGREP_EDITING_SURFACE_HPP
