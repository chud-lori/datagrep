#include "ConflictResolution.hpp"

#include <QFont>
#include <QFrame>
#include <QGridLayout>
#include <QHBoxLayout>
#include <QLabel>
#include <QPushButton>
#include <QScrollArea>
#include <QStringList>
#include <QVBoxLayout>

#include <algorithm>

namespace dg {

bool ConflictField::movedUnderneath() const {
    switch (server.kind) {
        case ServerValue::Kind::Value:
            return !loaded || server.value != *loaded;
        case ServerValue::Kind::Nested:
            return true;
        case ServerValue::Kind::Missing:
            return loaded.has_value();
    }
    return false;
}

int ConflictDocument::contestedCount() const {
    int n = 0;
    for (const ConflictField& f : fields) {
        if (f.movedUnderneath()) {
            ++n;
        }
    }
    return n;
}

ConflictReview ConflictReview::build(const QVector<StagedDocument>& conflicted,
                                     const QVector<ServerDocument>& server,
                                     const EditableResult& editable) {
    ConflictReview review;
    const int count = std::min(conflicted.size(), server.size());
    for (int i = 0; i < count; ++i) {
        const StagedDocument& staged = conflicted[i];
        const ServerDocument& now = server[i];
        ConflictDocument doc;
        doc.id = staged.id;
        QStringList titleParts;
        for (const FieldValue& fv : staged.key) {
            titleParts << fv.field + QLatin1Char('=') + fv.value.display();
        }
        // The engine's own field names and values, joined — never a guess at
        // which one is "the id".
        doc.title = titleParts.join(QStringLiteral("  "));
        for (const StagedField& set : staged.sets) {
            ConflictField field;
            field.name = set.field;
            field.loaded = set.loaded;
            field.server = now.fields.contains(set.field)
                               ? now.fields.value(set.field)
                               : ServerValue{};
            field.typed = set.value;
            doc.fields.append(field);
        }
        doc.isDelete = staged.isDelete;
        // The fresh guard, read out of the envelope by the field names the
        // engine named — this layer never learns what `_seq_no` is.
        bool guardComplete = true;
        for (const QString& field : editable.guardFields) {
            const auto value = now.envelope.contains(field)
                                   ? now.envelope.value(field).mutationValue()
                                   : std::nullopt;
            if (!value) {
                guardComplete = false;
                break;
            }
            doc.rebaseGuard.append(FieldValue{field, *value});
        }
        if (!now.found || !guardComplete || doc.rebaseGuard.isEmpty()) {
            doc.rebaseGuard.clear();
            doc.canRebase = false;
        } else {
            doc.canRebase = true;
        }
        doc.gone = !now.found && now.error.isEmpty();
        doc.error = now.error;
        review.documents.append(doc);
    }
    return review;
}

}  // namespace dg

// --- the dialog -------------------------------------------------------------

namespace {

QLabel* note(const QString& text, bool warning, QWidget* parent) {
    auto* label = new QLabel(
        (warning ? QStringLiteral("⚠ ") : QStringLiteral("ⓘ ")) + text, parent);
    label->setWordWrap(true);
    label->setTextFormat(Qt::PlainText);
    return label;
}

QLabel* cell(const QString& text, bool tinted, QWidget* parent) {
    auto* label = new QLabel(text, parent);
    QFont mono = label->font();
    mono.setFamily(QStringLiteral("monospace"));
    label->setFont(mono);
    label->setTextFormat(Qt::PlainText);
    label->setToolTip(text);
    if (tinted) {
        // The middle column tinted when it moved: that is the whole reason
        // this view exists, and it should be findable at a glance.
        label->setStyleSheet(QStringLiteral("color: rgb(200, 110, 10);"));
    }
    return label;
}

}  // namespace

ConflictReviewDialog::ConflictReviewDialog(const dg::ConflictReview& review,
                                           QWidget* parent)
    : QDialog(parent) {
    setWindowTitle(QStringLiteral("Resolve conflicts"));
    resize(720, 480);

    auto* layout = new QVBoxLayout(this);

    auto* title = new QLabel(
        review.documents.size() == 1
            ? QStringLiteral("1 document changed after you loaded it")
            : QStringLiteral("%1 documents changed after you loaded them")
                  .arg(review.documents.size()),
        this);
    QFont bold = title->font();
    bold.setBold(true);
    title->setFont(bold);
    layout->addWidget(title);

    auto* subtitle = new QLabel(
        QStringLiteral(
            "Nothing was written for these. Each one is shown as you loaded it, "
            "as the server holds it now, and as you typed it — so you can "
            "re-apply your edits onto the current version, or drop them."),
        this);
    subtitle->setWordWrap(true);
    layout->addWidget(subtitle);

    auto* list = new QWidget(this);
    listLayout_ = new QVBoxLayout(list);
    listLayout_->setContentsMargins(4, 4, 4, 4);
    listLayout_->setSpacing(14);
    for (const dg::ConflictDocument& document : review.documents) {
        QWidget* block = buildDocumentBlock(document);
        blocks_.insert(document.id, block);
        listLayout_->addWidget(block);
    }
    listLayout_->addStretch(1);

    auto* scroll = new QScrollArea(this);
    scroll->setWidgetResizable(true);
    scroll->setWidget(list);
    layout->addWidget(scroll, 1);

    auto* footer = new QHBoxLayout();
    auto* footNote = new QLabel(
        QStringLiteral("Anything left unresolved stays staged and unwritten."), this);
    footer->addWidget(footNote, 1);
    auto* closeButton = new QPushButton(QStringLiteral("Close"), this);
    closeButton->setDefault(true);
    connect(closeButton, &QPushButton::clicked, this, &QDialog::accept);
    footer->addWidget(closeButton);
    layout->addLayout(footer);
}

void ConflictReviewDialog::removeDocument(const QString& id) {
    QWidget* block = blocks_.take(id);
    if (block != nullptr) {
        listLayout_->removeWidget(block);
        block->deleteLater();
    }
    // An empty conflict view is nothing to read.
    if (blocks_.isEmpty()) {
        accept();
    }
}

QWidget* ConflictReviewDialog::buildDocumentBlock(
    const dg::ConflictDocument& document) {
    auto* block = new QWidget(this);
    auto* layout = new QVBoxLayout(block);
    layout->setContentsMargins(0, 0, 0, 0);
    layout->setSpacing(6);

    auto* head = new QHBoxLayout();
    auto* titleLabel = new QLabel(QStringLiteral("⑂ ") + document.title, block);
    QFont mono = titleLabel->font();
    mono.setFamily(QStringLiteral("monospace"));
    titleLabel->setFont(mono);
    head->addWidget(titleLabel);
    if (document.isDelete) {
        auto* tag = new QLabel(QStringLiteral("staged for deletion"), block);
        tag->setStyleSheet(QStringLiteral("color: rgb(200, 40, 40);"));
        head->addWidget(tag);
    }
    head->addStretch(1);
    layout->addLayout(head);

    if (!document.error.isEmpty()) {
        layout->addWidget(note(document.error, true, block));
    } else if (document.gone) {
        layout->addWidget(note(
            QStringLiteral(
                "This document is no longer on the server — somebody deleted it. "
                "There is no version to re-apply your edits onto."),
            true, block));
    }

    if (!document.fields.isEmpty()) {
        auto* grid = new QGridLayout();
        grid->setHorizontalSpacing(16);
        grid->setVerticalSpacing(3);
        const QStringList heads = {
            QStringLiteral("field"), QStringLiteral("you loaded"),
            QStringLiteral("on the server now"), QStringLiteral("you typed")};
        for (int c = 0; c < heads.size(); ++c) {
            auto* h = new QLabel(heads[c], block);
            QFont small = h->font();
            small.setBold(true);
            h->setFont(small);
            grid->addWidget(h, 0, c);
        }
        for (int r = 0; r < document.fields.size(); ++r) {
            const dg::ConflictField& field = document.fields[r];
            grid->addWidget(cell(field.name, false, block), r + 1, 0);
            grid->addWidget(
                cell(field.loaded ? field.loaded->display() : QStringLiteral("—"),
                     false, block),
                r + 1, 1);
            grid->addWidget(
                cell(field.server.display(), field.movedUnderneath(), block),
                r + 1, 2);
            grid->addWidget(cell(field.typed.display(), false, block), r + 1, 3);
        }
        grid->setColumnStretch(3, 1);
        layout->addLayout(grid);
    } else if (document.isDelete) {
        layout->addWidget(note(
            QStringLiteral(
                "A delete has no fields of its own. Re-applying it means deleting "
                "whatever the document is now, including the change somebody else "
                "just made."),
            false, block));
    }

    if (!document.gone && document.error.isEmpty()) {
        // The one sentence that decides which button is right.
        const int contested = document.contestedCount();
        QString summary;
        if (contested == 0) {
            summary = QStringLiteral(
                "The fields you edited are unchanged — somebody changed this "
                "document elsewhere. Re-applying writes your edits onto their "
                "version and overwrites nothing of theirs.");
        } else if (contested == 1) {
            summary = QStringLiteral(
                "1 of the fields you edited was changed by somebody else. "
                "Re-applying overwrites their value with yours.");
        } else {
            summary = QStringLiteral(
                          "%1 of the fields you edited were changed by somebody "
                          "else. Re-applying overwrites their values with yours.")
                          .arg(contested);
        }
        layout->addWidget(note(summary, contested > 0, block));
    }

    auto* choices = new QHBoxLayout();
    choices->addStretch(1);
    auto* discardButton = new QPushButton(QStringLiteral("Discard Mine"), block);
    const QString id = document.id;
    connect(discardButton, &QPushButton::clicked, this,
            [this, id]() { emit discardChosen(id); });
    choices->addWidget(discardButton);
    auto* rebaseButton = new QPushButton(
        document.isDelete ? QStringLiteral("Delete It Anyway")
                          : QStringLiteral("Re-apply Onto This Version"),
        block);
    rebaseButton->setEnabled(document.canRebase);
    connect(rebaseButton, &QPushButton::clicked, this,
            [this, id]() { emit rebaseChosen(id); });
    choices->addWidget(rebaseButton);
    layout->addLayout(choices);

    auto* rule = new QFrame(block);
    rule->setFrameShape(QFrame::HLine);
    layout->addWidget(rule);
    return block;
}
