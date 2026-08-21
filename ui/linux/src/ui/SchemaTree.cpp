#include "SchemaTree.hpp"

#include "ffi/DatagrepFfi.hpp"

#include <QInputDialog>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonValue>
#include <QLineEdit>
#include <QStringList>
#include <QTreeWidgetItem>

namespace {
// Item data roles carried on each tree node.
constexpr int kPathRole = Qt::UserRole + 1;         // QStringList: full path
constexpr int kHasChildrenRole = Qt::UserRole + 2;  // bool
constexpr int kEnumerationRole = Qt::UserRole + 3;  // QString
constexpr int kLoadedRole = Qt::UserRole + 4;       // bool: children fetched
constexpr int kScanPromptRole = Qt::UserRole + 5;   // bool: the "enter prefix" row
constexpr int kDescribedRole = Qt::UserRole + 6;    // bool: describe() already done
constexpr int kDescribeJsonRole = Qt::UserRole + 7;   // QString: raw describe JSON
constexpr int kDescribeErrorRole = Qt::UserRole + 8;  // QString: describe failure

const QString kScanOnly = QStringLiteral("scan_only");
const QString kCheap = QStringLiteral("cheap");

// A short human summary of one describe() payload, for a node's tooltip.
QString describeSummary(const QString& json) {
    const QJsonObject o = QJsonDocument::fromJson(json.toUtf8()).object();
    QStringList parts;
    const QString kind = o.value(QStringLiteral("kind")).toString();
    if (!kind.isEmpty()) {
        parts << kind;
    }
    const QJsonValue rows = o.value(QStringLiteral("row_estimate"));
    if (rows.isDouble()) {
        // "≈" because a cheap estimate is never a COUNT(*).
        parts << QStringLiteral("≈ %1 rows").arg(
            static_cast<qlonglong>(rows.toDouble()));
    }
    const QJsonValue cols = o.value(QStringLiteral("columns"));
    if (cols.isArray()) {
        const bool inferred = o.value(QStringLiteral("inferred")).toBool(false);
        parts << QStringLiteral("%1 %2")
                     .arg(static_cast<int>(cols.toArray().size()))
                     .arg(inferred ? QStringLiteral("fields (sampled)")
                                   : QStringLiteral("columns"));
    }
    const QString comment = o.value(QStringLiteral("comment")).toString();
    QString summary = parts.join(QStringLiteral(" · "));
    if (!comment.isEmpty()) {
        summary += QStringLiteral("\n%1").arg(comment);
    }
    return summary;
}

}  // namespace

SchemaTree::SchemaTree(QWidget* parent) : QTreeWidget(parent) {
    setHeaderHidden(true);
    setColumnCount(1);
    setUniformRowHeights(true);
    setExpandsOnDoubleClick(false);
    connect(this, &QTreeWidget::itemExpanded, this, &SchemaTree::onItemExpanded);
    connect(this, &QTreeWidget::itemActivated, this, &SchemaTree::onItemActivated);
    connect(this, &QTreeWidget::currentItemChanged, this,
            &SchemaTree::onCurrentItemChanged);
}

void SchemaTree::showProfile(const QString& profile) {
    profile_ = profile;
    clear();
    if (core_ == nullptr || profile_.isEmpty()) {
        return;
    }
    fetchInto(nullptr, QStringList());  // roots: path []
}

QString SchemaTree::encodePath(const QStringList& segments) {
    QJsonArray arr;
    for (const QString& s : segments) {
        arr.append(s);
    }
    return QString::fromUtf8(QJsonDocument(arr).toJson(QJsonDocument::Compact));
}

void SchemaTree::fetchInto(QTreeWidgetItem* parent, const QStringList& fetchPath) {
    if (core_ == nullptr) {
        return;
    }

    QString json;
    try {
        json = QString::fromStdString(core_->catalogChildrenJson(
            profile_.toStdString(), encodePath(fetchPath).toStdString()));
    } catch (const dg::Error& e) {
        // Surface the failure inline rather than silently showing an empty node.
        auto* err = parent != nullptr ? new QTreeWidgetItem(parent)
                                      : new QTreeWidgetItem(this);
        err->setText(0, QStringLiteral("⚠ %1").arg(QString::fromUtf8(e.what())));
        err->setFlags(Qt::ItemIsEnabled);
        return;
    }

    const QJsonArray children = QJsonDocument::fromJson(json.toUtf8()).array();
    for (const QJsonValue& v : children) {
        const QJsonObject o = v.toObject();
        const QString name = o.value(QStringLiteral("name")).toString();
        const QString kind = o.value(QStringLiteral("kind")).toString();
        const bool hasChildren = o.value(QStringLiteral("has_children")).toBool(false);
        const QString enumeration = o.value(QStringLiteral("enumeration")).toString();

        QStringList childPath = fetchPath;
        childPath << name;

        auto* node = parent != nullptr ? new QTreeWidgetItem(parent)
                                       : new QTreeWidgetItem(this);
        node->setText(0, name);
        node->setToolTip(0, kind);
        node->setData(0, kPathRole, childPath);
        node->setData(0, kHasChildrenRole, hasChildren);
        node->setData(0, kEnumerationRole, enumeration);
        node->setData(0, kLoadedRole, false);
        node->setData(0, kDescribedRole, false);

        if (hasChildren) {
            if (enumeration == kScanOnly) {
                // No cheap listing: enumerating would be a full keyspace scan, so
                // instead of a fetch-on-expand placeholder we plant a prompt row
                // the user activates to supply a prefix. Expanding the node alone
                // never enumerates it.
                auto* prompt = new QTreeWidgetItem(node);
                prompt->setText(0, QStringLiteral("Double-click to enter a key prefix…"));
                prompt->setData(0, kScanPromptRole, true);
                prompt->setFlags(Qt::ItemIsEnabled | Qt::ItemIsSelectable);
                prompt->setToolTip(
                    0, QStringLiteral("This node has no cheap listing. Enter a key "
                                      "prefix — enumerating everything would be a "
                                      "full keyspace scan."));
            } else {
                // A lazy placeholder gives the expand arrow without fetching the
                // next level yet. It is replaced on first expansion.
                auto* placeholder = new QTreeWidgetItem(node);
                placeholder->setText(0, QStringLiteral("…"));
                placeholder->setFlags(Qt::NoItemFlags);
                // Only "cheap" nodes may auto-expand; paged / on_demand wait for
                // the user so a large listing is never crawled on tree open.
                if (enumeration == kCheap) {
                    node->setExpanded(true);  // triggers onItemExpanded -> fetch
                }
            }
        }
    }
    if (parent != nullptr) {
        parent->setData(0, kLoadedRole, true);
    }
}

void SchemaTree::onItemExpanded(QTreeWidgetItem* item) {
    if (item == nullptr || item->data(0, kLoadedRole).toBool()) {
        return;  // already fetched
    }
    // A scan_only node is NEVER enumerated by expansion — it waits for a prefix.
    // Expanding it just reveals its "enter a prefix" prompt row.
    if (item->data(0, kEnumerationRole).toString() == kScanOnly) {
        return;
    }
    // Drop the lazy placeholder(s), then fetch the real level.
    const auto placeholders = item->takeChildren();
    for (QTreeWidgetItem* p : placeholders) {
        delete p;
    }
    fetchInto(item, item->data(0, kPathRole).toStringList());
}

void SchemaTree::promptScan(QTreeWidgetItem* node) {
    if (node == nullptr || core_ == nullptr) {
        return;
    }
    bool ok = false;
    const QString prefix = QInputDialog::getText(
        this, QStringLiteral("Scan required"),
        QStringLiteral("‘%1’ has no cheap listing. Enter a key prefix — "
                       "enumerating everything would be a full keyspace scan.")
            .arg(node->text(0)),
        QLineEdit::Normal, QString(), &ok);
    const QString trimmed = prefix.trimmed();
    if (!ok || trimmed.isEmpty()) {
        return;
    }
    // Drop the prompt row(s), then enumerate node-path + [prefix] under the node.
    const auto rows = node->takeChildren();
    for (QTreeWidgetItem* r : rows) {
        delete r;
    }
    QStringList fetchPath = node->data(0, kPathRole).toStringList();
    fetchPath << trimmed;
    fetchInto(node, fetchPath);
    node->setExpanded(true);
}

void SchemaTree::onItemActivated(QTreeWidgetItem* item, int /*column*/) {
    if (item == nullptr) {
        return;
    }
    // Activating the "enter a prefix" prompt row scans its parent node.
    if (item->data(0, kScanPromptRole).toBool()) {
        promptScan(item->parent());
        return;
    }
    const QStringList path = item->data(0, kPathRole).toStringList();
    if (path.isEmpty()) {
        return;
    }
    emit objectActivated(profile_, encodePath(path));
}

void SchemaTree::onCurrentItemChanged(QTreeWidgetItem* current,
                                      QTreeWidgetItem* /*previous*/) {
    // Lazily describe the selected object into its tooltip — one object, only on
    // selection, and only once (cached via kDescribedRole). Never on expansion,
    // so selecting a node reads its columns/indexes but opening the tree does not.
    // The raw payload is kept on the node and re-announced on every selection,
    // so the inspector's schema pane follows the selection without ever issuing
    // a describe of its own.
    if (core_ == nullptr) {
        return;
    }
    if (current == nullptr || current->data(0, kScanPromptRole).toBool()) {
        emit objectDescribed(profile_, QString(), QString(), QString());
        return;
    }
    const QStringList path = current->data(0, kPathRole).toStringList();
    if (path.isEmpty()) {
        emit objectDescribed(profile_, QString(), QString(), QString());
        return;
    }
    if (current->data(0, kDescribedRole).toBool()) {
        // Cached — re-announce without re-describing.
        emit objectDescribed(profile_, encodePath(path),
                             current->data(0, kDescribeJsonRole).toString(),
                             current->data(0, kDescribeErrorRole).toString());
        return;
    }
    current->setData(0, kDescribedRole, true);  // set first: don't retry on failure loops
    try {
        const QString json = QString::fromStdString(core_->catalogDescribeJson(
            profile_.toStdString(), encodePath(path).toStdString()));
        const QString summary = describeSummary(json);
        if (!summary.isEmpty()) {
            current->setToolTip(0, summary);
        }
        current->setData(0, kDescribeJsonRole, json);
        emit objectDescribed(profile_, encodePath(path), json, QString());
    } catch (const dg::Error& e) {
        // A describe failure is not worth interrupting selection over; the node
        // simply keeps its kind tooltip. Leave kDescribedRole true so a broken
        // object is not re-hit on every reselection. The pane still learns WHY.
        const QString error = QString::fromUtf8(e.what());
        current->setData(0, kDescribeErrorRole, error);
        emit objectDescribed(profile_, encodePath(path), QString(), error);
    }
}
