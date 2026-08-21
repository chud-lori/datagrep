#include "DetailPanel.hpp"

#include <QApplication>
#include <QClipboard>
#include <QFont>
#include <QFontDatabase>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonValue>
#include <QLabel>
#include <QLocale>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QStringList>
#include <QTreeWidget>
#include <QTreeWidgetItem>
#include <QVBoxLayout>
#include <QWidget>

namespace {

// Pretty-print a JSON payload for reading. A cell's detail can be a bare
// scalar ("abc", 42), which QJsonDocument refuses as a document — those are
// shown verbatim, which for a scalar IS the pretty form.
QString prettyJson(const QString& json) {
    const QJsonDocument doc = QJsonDocument::fromJson(json.toUtf8());
    if (doc.isNull()) {
        return json.trimmed();
    }
    return QString::fromUtf8(doc.toJson(QJsonDocument::Indented)).trimmed();
}

// "12.3 MB" from a byte count. One decimal, binary steps — a size a person
// compares, not audits.
QString humanBytes(double bytes) {
    static const char* kUnits[] = {"B", "KB", "MB", "GB", "TB"};
    int unit = 0;
    while (bytes >= 1024.0 && unit < 4) {
        bytes /= 1024.0;
        ++unit;
    }
    return unit == 0 ? QStringLiteral("%1 B").arg(static_cast<qlonglong>(bytes))
                     : QStringLiteral("%1 %2")
                           .arg(bytes, 0, 'f', 1)
                           .arg(QLatin1String(kUnits[unit]));
}

// One section header row ("Columns (12)", "Indexes — not reported"). Bold so
// the tree reads as sections without needing a second widget per group.
QTreeWidgetItem* sectionItem(QTreeWidget* tree, const QString& text) {
    auto* item = new QTreeWidgetItem(tree);
    item->setText(0, text);
    item->setFirstColumnSpanned(true);
    QFont f = item->font(0);
    f.setBold(true);
    item->setFont(0, f);
    item->setFlags(Qt::ItemIsEnabled);
    return item;
}

// The details string for one column of the described object: the most specific
// type the engine reported, then the facts as short markers. Only facts that
// ARRIVED are printed — an absent field is not a false one.
QString columnDetails(const QJsonObject& c) {
    QStringList parts;
    QString type = c.value(QStringLiteral("native_type")).toString();
    if (type.isEmpty()) {
        type = c.value(QStringLiteral("logical_type")).toString();
    }
    if (type.isEmpty()) {
        type = c.value(QStringLiteral("type")).toString();
    }
    if (!type.isEmpty()) {
        parts << type;
    }
    if (c.value(QStringLiteral("primary_key")).toBool(false)) {
        parts << QStringLiteral("primary key");
    }
    if (c.value(QStringLiteral("unique")).toBool(false)) {
        parts << QStringLiteral("unique");
    }
    if (c.value(QStringLiteral("indexed")).toBool(false)) {
        parts << QStringLiteral("indexed");
    }
    const QJsonValue nullable = c.value(QStringLiteral("nullable"));
    if (nullable.isBool() && !nullable.toBool()) {
        parts << QStringLiteral("not null");
    }
    if (c.value(QStringLiteral("auto_generated")).toBool(false)) {
        parts << QStringLiteral("auto");
    }
    const QJsonValue def = c.value(QStringLiteral("default"));
    if (def.isString() && !def.toString().isEmpty()) {
        parts << QStringLiteral("default %1").arg(def.toString());
    }
    // Sampled documents only: how often the field was actually present.
    const QJsonValue presence = c.value(QStringLiteral("presence_ratio"));
    if (presence.isDouble()) {
        parts << QStringLiteral("in %1% of sampled docs")
                     .arg(static_cast<int>(presence.toDouble() * 100.0));
    }
    return parts.join(QStringLiteral(" · "));
}

// The details string for one index: its column list with per-column order,
// then its properties.
QString indexDetails(const QJsonObject& ix) {
    QStringList cols;
    const QJsonArray columns = ix.value(QStringLiteral("columns")).toArray();
    for (const QJsonValue& v : columns) {
        const QJsonObject c = v.toObject();
        QString col = c.value(QStringLiteral("name")).toString();
        const QString order = c.value(QStringLiteral("order")).toString();
        if (!order.isEmpty()) {
            col += QStringLiteral(" %1").arg(order);
        }
        cols << col;
    }
    QStringList parts;
    if (!cols.isEmpty()) {
        parts << cols.join(QStringLiteral(", "));
    }
    if (ix.value(QStringLiteral("primary")).toBool(false)) {
        parts << QStringLiteral("primary");
    }
    if (ix.value(QStringLiteral("unique")).toBool(false)) {
        parts << QStringLiteral("unique");
    }
    const QString type = ix.value(QStringLiteral("type")).toString();
    if (!type.isEmpty()) {
        parts << type;
    }
    if (ix.value(QStringLiteral("sparse")).toBool(false)) {
        parts << QStringLiteral("sparse");
    }
    if (ix.value(QStringLiteral("partial")).toBool(false)) {
        const QString filter = ix.value(QStringLiteral("filter")).toString();
        parts << (filter.isEmpty()
                      ? QStringLiteral("partial")
                      : QStringLiteral("partial: %1").arg(filter));
    }
    const QJsonValue expire = ix.value(QStringLiteral("expire_after_seconds"));
    if (expire.isDouble()) {
        parts << QStringLiteral("expires after %1 s")
                     .arg(static_cast<qlonglong>(expire.toDouble()));
    }
    const QJsonValue size = ix.value(QStringLiteral("size_bytes"));
    if (size.isDouble()) {
        parts << humanBytes(size.toDouble());
    }
    return parts.join(QStringLiteral(" · "));
}

}  // namespace

DetailPanel::DetailPanel(QWidget* parent) : QTabWidget(parent) {
    setDocumentMode(true);
    buildSchemaTab();
    buildCellTab();
    // Start on Schema: it is the tab that has content before any query has run.
    setCurrentIndex(0);
}

void DetailPanel::buildSchemaTab() {
    auto* page = new QWidget(this);
    auto* layout = new QVBoxLayout(page);
    layout->setContentsMargins(8, 8, 8, 8);
    layout->setSpacing(4);

    schemaTitle_ = new QLabel(QStringLiteral("Schema"), page);
    QFont titleFont = schemaTitle_->font();
    titleFont.setBold(true);
    schemaTitle_->setFont(titleFont);

    schemaSubtitle_ = new QLabel(
        QStringLiteral("Select a table, view, collection or key in the sidebar "
                       "to see its structure."),
        page);
    schemaSubtitle_->setWordWrap(true);
    schemaSubtitle_->setStyleSheet(QStringLiteral("color: gray; font-size: 11px;"));

    schemaStats_ = new QLabel(page);
    schemaStats_->setWordWrap(true);
    schemaStats_->setStyleSheet(QStringLiteral("color: gray; font-size: 11px;"));
    schemaStats_->hide();

    schemaTree_ = new QTreeWidget(page);
    schemaTree_->setColumnCount(2);
    schemaTree_->setHeaderHidden(true);
    schemaTree_->setUniformRowHeights(true);
    schemaTree_->setRootIsDecorated(false);
    schemaTree_->setIndentation(12);
    schemaTree_->header()->setSectionResizeMode(0, QHeaderView::ResizeToContents);
    schemaTree_->header()->setSectionResizeMode(1, QHeaderView::Stretch);

    layout->addWidget(schemaTitle_);
    layout->addWidget(schemaSubtitle_);
    layout->addWidget(schemaStats_);
    layout->addWidget(schemaTree_, 1);

    addTab(page, QStringLiteral("Schema"));
}

void DetailPanel::buildCellTab() {
    auto* page = new QWidget(this);
    auto* layout = new QVBoxLayout(page);
    layout->setContentsMargins(8, 8, 8, 8);
    layout->setSpacing(4);

    auto* header = new QWidget(page);
    auto* headerLayout = new QHBoxLayout(header);
    headerLayout->setContentsMargins(0, 0, 0, 0);
    headerLayout->setSpacing(4);

    cellTitle_ = new QLabel(QStringLiteral("nothing selected"), header);
    cellTitle_->setStyleSheet(QStringLiteral("color: gray; font-size: 11px;"));

    cellCopyButton_ = new QPushButton(QStringLiteral("Copy JSON"), header);
    cellCopyButton_->setEnabled(false);
    connect(cellCopyButton_, &QPushButton::clicked, this, [this]() {
        QApplication::clipboard()->setText(cellText_->toPlainText());
        emit cellCopied();
    });

    headerLayout->addWidget(cellTitle_, 1);
    headerLayout->addWidget(cellCopyButton_);

    cellText_ = new QPlainTextEdit(page);
    cellText_->setReadOnly(true);
    cellText_->setLineWrapMode(QPlainTextEdit::NoWrap);
    cellText_->setFont(QFontDatabase::systemFont(QFontDatabase::FixedFont));
    cellText_->setPlaceholderText(
        QStringLiteral("Click a cell in the grid to see its whole value — a "
                       "{…} chip opens here on its own."));

    // The product's honesty claim, spelled out where it can be read: a field
    // missing from a document is a different fact from a field that is null,
    // and both are different from an empty string. Same legend as the macOS
    // cell pane.
    auto* legend = new QLabel(
        QStringLiteral("NULL — present, and null\n"
                       "(empty) — present, empty string\n"
                       "— — ABSENT: not in the document at all\n"
                       "{n fields} — nested: click to open here"),
        page);
    legend->setStyleSheet(QStringLiteral("color: gray; font-size: 10px;"));

    layout->addWidget(header);
    layout->addWidget(cellText_, 1);
    layout->addWidget(legend);

    addTab(page, QStringLiteral("Cell"));
}

void DetailPanel::showSchema(const QString& /*profile*/, const QString& pathJson,
                             const QString& describeJson, const QString& error) {
    schemaTree_->clear();
    schemaStats_->hide();

    if (pathJson.isEmpty()) {
        schemaTitle_->setText(QStringLiteral("Schema"));
        schemaSubtitle_->setText(
            QStringLiteral("Select a table, view, collection or key in the "
                           "sidebar to see its structure."));
        return;
    }

    // Title = the object's leaf name; subtitle = where it lives.
    const QJsonArray path = QJsonDocument::fromJson(pathJson.toUtf8()).array();
    QStringList segments;
    for (const QJsonValue& v : path) {
        segments << v.toString();
    }
    schemaTitle_->setText(segments.isEmpty() ? QStringLiteral("Schema")
                                             : segments.last());
    segments.removeLast();
    schemaSubtitle_->setText(segments.join(QStringLiteral(" › ")));

    if (!error.isEmpty()) {
        schemaStats_->setText(QStringLiteral("⚠ describe failed: %1").arg(error));
        schemaStats_->show();
        return;
    }
    if (describeJson.isEmpty()) {
        return;
    }

    const QJsonObject o = QJsonDocument::fromJson(describeJson.toUtf8()).object();

    // --- the stats strip: only facts that arrived -----------------------------
    QStringList stats;
    const QString kind = o.value(QStringLiteral("kind")).toString();
    if (!kind.isEmpty()) {
        stats << kind;
    }
    const QJsonValue rows = o.value(QStringLiteral("row_estimate"));
    if (rows.isDouble()) {
        // "≈" because a cheap estimate is never a COUNT(*).
        stats << QStringLiteral("≈ %1 rows")
                     .arg(QLocale().toString(
                         static_cast<qlonglong>(rows.toDouble())));
    }
    const QJsonValue size = o.value(QStringLiteral("size_bytes"));
    if (size.isDouble()) {
        stats << humanBytes(size.toDouble());
    }
    if (o.value(QStringLiteral("inferred")).toBool(false)) {
        const QJsonValue sampled = o.value(QStringLiteral("sampled_docs"));
        stats << (sampled.isDouble()
                      ? QStringLiteral("inferred from %1 sampled docs")
                            .arg(static_cast<qlonglong>(sampled.toDouble()))
                      : QStringLiteral("inferred"));
    }
    QString statsText = stats.join(QStringLiteral(" · "));
    const QString comment = o.value(QStringLiteral("comment")).toString();
    if (!comment.isEmpty()) {
        statsText += statsText.isEmpty() ? comment
                                         : QStringLiteral("\n%1").arg(comment);
    }
    if (!statsText.isEmpty()) {
        schemaStats_->setText(statsText);
        schemaStats_->show();
    }

    // --- columns: [] and null are two different sentences ---------------------
    const QJsonValue columnsValue = o.value(QStringLiteral("columns"));
    if (columnsValue.isArray()) {
        const QJsonArray columns = columnsValue.toArray();
        if (columns.isEmpty()) {
            sectionItem(schemaTree_, QStringLiteral("Columns — none"));
        } else {
            auto* section = sectionItem(
                schemaTree_,
                QStringLiteral("Columns (%1)").arg(columns.size()));
            for (const QJsonValue& v : columns) {
                const QJsonObject c = v.toObject();
                auto* item = new QTreeWidgetItem(section);
                item->setText(0, c.value(QStringLiteral("name")).toString());
                item->setText(1, columnDetails(c));
                item->setToolTip(1, item->text(1));
            }
            section->setExpanded(true);
        }
    } else {
        sectionItem(schemaTree_, QStringLiteral("Columns — not reported"));
    }

    // --- indexes: the same distinction --------------------------------------
    const QJsonValue indexesValue = o.value(QStringLiteral("indexes"));
    if (indexesValue.isArray()) {
        const QJsonArray indexes = indexesValue.toArray();
        if (indexes.isEmpty()) {
            sectionItem(schemaTree_, QStringLiteral("Indexes — none"));
        } else {
            auto* section = sectionItem(
                schemaTree_,
                QStringLiteral("Indexes (%1)").arg(indexes.size()));
            for (const QJsonValue& v : indexes) {
                const QJsonObject ix = v.toObject();
                auto* item = new QTreeWidgetItem(section);
                item->setText(0, ix.value(QStringLiteral("name")).toString());
                item->setText(1, indexDetails(ix));
                item->setToolTip(1, item->text(1));
                const QString definition =
                    ix.value(QStringLiteral("definition")).toString();
                if (!definition.isEmpty()) {
                    item->setToolTip(0, definition);
                }
            }
            section->setExpanded(true);
        }
    } else {
        sectionItem(schemaTree_, QStringLiteral("Indexes — not reported"));
    }

    // --- whatever else the driver reported, shown rather than dropped ---------
    const QJsonObject extra = o.value(QStringLiteral("extra")).toObject();
    if (!extra.isEmpty()) {
        auto* section = sectionItem(schemaTree_, QStringLiteral("Extra"));
        for (auto it = extra.constBegin(); it != extra.constEnd(); ++it) {
            auto* item = new QTreeWidgetItem(section);
            item->setText(0, it.key());
            item->setText(1, it.value().toVariant().toString());
        }
    }
}

void DetailPanel::showCell(int row, int column, const QString& detailJson,
                           const QString& envelopeJson, bool raise) {
    cellTitle_->setText(
        QStringLiteral("row %1 · column %2").arg(row + 1).arg(column + 1));

    const QString value = detailJson.isEmpty()
                              ? QStringLiteral("(no detail available)")
                              : prettyJson(detailJson);
    // On a result with a projection root the columns are the document's own
    // fields, so which document a value belongs to is no longer visible in the
    // grid — and this pane is exactly where someone comes to ask. The envelope
    // leads, because it is the answer.
    const QJsonObject envelope =
        QJsonDocument::fromJson(envelopeJson.toUtf8()).object();
    if (!envelope.isEmpty()) {
        cellText_->setPlainText(QStringLiteral("// document\n%1\n\n// value\n%2")
                                    .arg(prettyJson(envelopeJson), value));
    } else {
        cellText_->setPlainText(value);
    }
    cellCopyButton_->setEnabled(!cellText_->toPlainText().isEmpty());
    if (raise) {
        setCurrentIndex(1);
    }
}

void DetailPanel::clearCell() {
    cellTitle_->setText(QStringLiteral("nothing selected"));
    cellText_->clear();
    cellCopyButton_->setEnabled(false);
}
