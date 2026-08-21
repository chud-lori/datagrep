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
    void resolveConflictsRequested();
};

#endif  // DATAGREP_EDITING_SURFACE_HPP
