#include "EditingSurface.hpp"

#include "model/GridEditing.hpp"

#include <QDialogButtonBox>
#include <QFrame>
#include <QHBoxLayout>
#include <QLabel>
#include <QPushButton>
#include <QScrollArea>
#include <QStringList>
#include <QVBoxLayout>

namespace {

QLabel* wrappedLabel(const QString& text, QWidget* parent) {
    auto* label = new QLabel(text, parent);
    label->setWordWrap(true);
    label->setTextFormat(Qt::PlainText);
    return label;
}

}  // namespace

// --- the bar ----------------------------------------------------------------

StagedEditsBar::StagedEditsBar(dg::PendingEdits* edits, QWidget* parent)
    : QWidget(parent), edits_(edits) {
    auto* layout = new QHBoxLayout(this);
    layout->setContentsMargins(12, 6, 12, 6);
    layout->setSpacing(10);

    headline_ = new QLabel(this);
    QFont bold = headline_->font();
    bold.setBold(true);
    headline_->setFont(bold);
    detail_ = new QLabel(this);
    detail_->setTextFormat(Qt::PlainText);

    auto* text = new QVBoxLayout();
    text->setContentsMargins(0, 0, 0, 0);
    text->setSpacing(1);
    text->addWidget(headline_);
    text->addWidget(detail_);
    layout->addLayout(text, 1);

    discardButton_ = new QPushButton(QStringLiteral("Discard"), this);
    resolveButton_ = new QPushButton(this);
    commitButton_ = new QPushButton(this);
    reloadButton_ = new QPushButton(QStringLiteral("Reload"), this);
    connect(discardButton_, &QPushButton::clicked, this,
            &StagedEditsBar::discardRequested);
    connect(resolveButton_, &QPushButton::clicked, this,
            &StagedEditsBar::resolveRequested);
    connect(commitButton_, &QPushButton::clicked, this,
            &StagedEditsBar::commitRequested);
    connect(reloadButton_, &QPushButton::clicked, this,
            &StagedEditsBar::reloadRequested);
    layout->addWidget(discardButton_);
    layout->addWidget(resolveButton_);
    layout->addWidget(commitButton_);
    layout->addWidget(reloadButton_);

    setAutoFillBackground(true);
    connect(edits_, &dg::PendingEdits::stagingChanged, this,
            &StagedEditsBar::refresh);
    refresh();
}

void StagedEditsBar::setCommitting(bool committing) {
    committing_ = committing;
    refresh();
}

void StagedEditsBar::setRereading(bool rereading) {
    rereading_ = rereading;
    refresh();
}

void StagedEditsBar::refresh() {
    if (edits_->isEmpty()) {
        hide();
        return;
    }
    const int pending = edits_->pendingCount();
    const int written = edits_->documents().size() - pending;
    const int conflicts = edits_->conflictCount();

    if (pending == 0) {
        // Everything staged was written. The grid still shows the rows as they
        // were loaded with the typed values drawn over them, so the only way
        // to see what the server holds is to ask it again — which is what the
        // reload button offers.
        headline_->setText(
            written == 1
                ? QStringLiteral("1 document written — the grid still shows what was loaded")
                : QStringLiteral("%1 documents written — the grid still shows what was loaded")
                      .arg(written));
    } else {
        headline_->setText(
            pending == 1
                ? QStringLiteral("1 document edited, not yet written")
                : QStringLiteral("%1 documents edited, not yet written").arg(pending));
    }

    // Updates and deletes are different enough that a single count would hide
    // one behind the other.
    QStringList parts;
    if (edits_->updateCount() > 0) {
        parts << QStringLiteral("%1 to update").arg(edits_->updateCount());
    }
    if (edits_->deleteCount() > 0) {
        parts << QStringLiteral("%1 to delete").arg(edits_->deleteCount());
    }
    if (written > 0) {
        parts << QStringLiteral("%1 already written").arg(written);
    }
    if (conflicts > 0) {
        parts << (conflicts == 1
                      ? QStringLiteral("1 changed on the server")
                      : QStringLiteral("%1 changed on the server").arg(conflicts));
    }
    detail_->setText(parts.join(QStringLiteral(" · ")));
    detail_->setVisible(!parts.isEmpty());

    if (committing_) {
        headline_->setText(QStringLiteral("committing…"));
        discardButton_->hide();
        resolveButton_->hide();
        commitButton_->hide();
        reloadButton_->hide();
    } else if (pending == 0) {
        discardButton_->hide();
        resolveButton_->hide();
        commitButton_->hide();
        reloadButton_->show();
    } else {
        discardButton_->show();
        // A conflicted edit cannot simply be committed again — the same guard
        // would refuse it — so the way forward is offered where the refusal is
        // visible, not only inside the report.
        resolveButton_->setVisible(conflicts > 0);
        resolveButton_->setText(
            conflicts == 1 ? QStringLiteral("Resolve 1 Conflict…")
                           : QStringLiteral("Resolve %1 Conflicts…").arg(conflicts));
        resolveButton_->setEnabled(!rereading_);
        commitButton_->show();
        commitButton_->setText(pending == 1
                                   ? QStringLiteral("Commit 1…")
                                   : QStringLiteral("Commit %1…").arg(pending));
        reloadButton_->hide();
    }
    show();
}

// --- the report -------------------------------------------------------------

MutationReportDialog::MutationReportDialog(const dg::MutationReport& report,
                                           QWidget* parent)
    : QDialog(parent) {
    setWindowTitle(QStringLiteral("Commit report"));
    resize(560, 420);

    auto* layout = new QVBoxLayout(this);

    const QString title =
        report.isClean()
            ? (report.applied == 1
                   ? QStringLiteral("1 document written")
                   : QStringLiteral("%1 documents written").arg(report.applied))
            : QStringLiteral("The batch stopped part way through");
    auto* titleLabel = new QLabel(title, this);
    QFont bold = titleLabel->font();
    bold.setBold(true);
    titleLabel->setFont(bold);
    layout->addWidget(titleLabel);

    QString subtitle = QStringLiteral("%1 applied").arg(report.applied);
    if (report.failed > 0) {
        subtitle += QStringLiteral(" · %1 failed").arg(report.failed);
    }
    if (report.notAttempted > 0) {
        subtitle += QStringLiteral(" · %1 never attempted").arg(report.notAttempted);
        subtitle += QStringLiteral(
            ". The ones that were never attempted are still staged — nothing was "
            "written for them, and nothing was lost.");
    }
    if (report.conflicts > 0) {
        subtitle += QStringLiteral(
            " A version conflict means the document changed on the server after "
            "you loaded it, so the write was refused rather than overwriting "
            "someone else's change. What you typed is still staged — resolve it "
            "to see what changed.");
    }
    layout->addWidget(wrappedLabel(subtitle, this));

    auto* list = new QWidget(this);
    auto* listLayout = new QVBoxLayout(list);
    listLayout->setContentsMargins(4, 4, 4, 4);
    listLayout->setSpacing(8);

    for (const dg::MutationNotice& notice : report.notices) {
        const QString prefix = notice.isWarning() ? QStringLiteral("⚠ ")
                                                  : QStringLiteral("ⓘ ");
        QString text = prefix + notice.message;
        if (!notice.code.isEmpty()) {
            text += QStringLiteral("  [%1]").arg(notice.code);
        }
        listLayout->addWidget(wrappedLabel(text, list));
    }
    if (!report.notices.isEmpty()) {
        auto* rule = new QFrame(list);
        rule->setFrameShape(QFrame::HLine);
        listLayout->addWidget(rule);
    }

    for (const dg::MutationRow& row : report.rows) {
        QString mark;
        QString detail;
        switch (row.outcome) {
            case dg::MutationRow::Outcome::Applied: {
                mark = QStringLiteral("✓");
                detail = QStringLiteral("written");
                if (row.seqNo) {
                    detail += QStringLiteral(" · now at _seq_no %1").arg(*row.seqNo);
                }
                if (row.forcedRefresh) {
                    detail += QStringLiteral(
                        " · the server forced an immediate refresh rather than "
                        "waiting for one");
                }
                break;
            }
            case dg::MutationRow::Outcome::NotAttempted:
                mark = QStringLiteral("…");
                detail = QStringLiteral(
                    "never attempted — the batch stopped before it, so this is "
                    "still staged");
                break;
            case dg::MutationRow::Outcome::Failed:
                mark = row.conflict ? QStringLiteral("⑂") : QStringLiteral("✗");
                detail = row.conflict
                             ? QStringLiteral(
                                   "version conflict — this document changed on the "
                                   "server after you loaded it, so nothing was written")
                             : (row.error.isEmpty()
                                    ? QStringLiteral("the write failed")
                                    : row.error);
                break;
        }
        listLayout->addWidget(wrappedLabel(
            QStringLiteral("%1  %2  %3/%4\n     %5")
                .arg(mark, row.op, row.index, row.documentId, detail),
            list));
    }
    listLayout->addStretch(1);

    auto* scroll = new QScrollArea(this);
    scroll->setWidgetResizable(true);
    scroll->setWidget(list);
    layout->addWidget(scroll, 1);

    const QString footer =
        report.conflicts > 0
            ? QStringLiteral("Reads each conflicted document back and shows what changed.")
            : (report.isClean()
                   ? QString()
                   : QStringLiteral("Re-run the statement to see what the server holds now."));
    if (!footer.isEmpty()) {
        layout->addWidget(wrappedLabel(footer, this));
    }

    auto* buttons = new QDialogButtonBox(this);
    if (report.conflicts > 0) {
        QPushButton* resolve = buttons->addButton(
            QStringLiteral("Resolve Conflicts…"), QDialogButtonBox::ActionRole);
        connect(resolve, &QPushButton::clicked, this, [this]() {
            emit resolveConflictsRequested();
            accept();
        });
    }
    QPushButton* done =
        buttons->addButton(QStringLiteral("Done"), QDialogButtonBox::AcceptRole);
    done->setDefault(true);
    connect(buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    layout->addWidget(buttons);
}
