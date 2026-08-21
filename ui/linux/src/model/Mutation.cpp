#include "Mutation.hpp"

#include <QJsonArray>
#include <QJsonDocument>
#include <QMetaType>
#include <QVariant>

namespace dg {

// --- MutationValue ---------------------------------------------------------

MutationValue MutationValue::str(const QString& s) {
    MutationValue v;
    v.kind_ = Kind::Str;
    v.s_ = s;
    return v;
}

MutationValue MutationValue::i64(qint64 i) {
    MutationValue v;
    v.kind_ = Kind::I64;
    v.i_ = i;
    return v;
}

MutationValue MutationValue::f64(double d) {
    MutationValue v;
    v.kind_ = Kind::F64;
    v.d_ = d;
    return v;
}

MutationValue MutationValue::boolean(bool b) {
    MutationValue v;
    v.kind_ = Kind::Bool;
    v.b_ = b;
    return v;
}

MutationValue MutationValue::null() { return MutationValue(); }

QJsonValue MutationValue::abiJson() const {
    switch (kind_) {
        case Kind::Str: return QJsonObject{{QStringLiteral("Str"), s_}};
        case Kind::I64: return QJsonObject{{QStringLiteral("I64"), i_}};
        case Kind::F64: return QJsonObject{{QStringLiteral("F64"), d_}};
        case Kind::Bool: return QJsonObject{{QStringLiteral("Bool"), b_}};
        case Kind::Null: return QJsonValue(QStringLiteral("Null"));
    }
    return QJsonValue(QStringLiteral("Null"));
}

namespace {

QString shortestDouble(double d) {
    for (int precision = 1; precision <= 17; ++precision) {
        const QString text = QString::number(d, 'g', precision);
        if (text.toDouble() == d) {
            return text;
        }
    }
    return QString::number(d, 'g', 17);
}

}  // namespace

QString MutationValue::display() const {
    switch (kind_) {
        case Kind::Str: return s_;
        case Kind::I64: return QString::number(i_);
        case Kind::F64: return shortestDouble(d_);
        case Kind::Bool: return b_ ? QStringLiteral("true") : QStringLiteral("false");
        case Kind::Null: return QStringLiteral("NULL");
    }
    return QString();
}

bool MutationValue::operator==(const MutationValue& other) const {
    if (kind_ != other.kind_) {
        return false;
    }
    switch (kind_) {
        case Kind::Str: return s_ == other.s_;
        case Kind::I64: return i_ == other.i_;
        case Kind::F64: return d_ == other.d_;
        case Kind::Bool: return b_ == other.b_;
        case Kind::Null: return true;
    }
    return false;
}

std::optional<MutationValue> MutationValue::decode(const QJsonValue& v) {
    switch (v.type()) {
        case QJsonValue::String: return str(v.toString());
        case QJsonValue::Null: return null();
        case QJsonValue::Bool: return boolean(v.toBool());
        case QJsonValue::Double: {
            const QVariant var = v.toVariant();
            if (var.typeId() == QMetaType::LongLong ||
                var.typeId() == QMetaType::ULongLong ||
                var.typeId() == QMetaType::Int) {
                return i64(var.toLongLong());
            }
            return f64(v.toDouble());
        }
        default: return std::nullopt;  // object / array / undefined
    }
}

std::optional<MutationValue> MutationValue::decodeFragment(const QString& json) {
    const QJsonDocument doc = QJsonDocument::fromJson(
        QByteArrayLiteral("[") + json.toUtf8() + QByteArrayLiteral("]"));
    if (!doc.isArray() || doc.array().size() != 1) {
        return std::nullopt;
    }
    return decode(doc.array().at(0));
}

bool MutationValue::typedLike(const QString& text,
                              const std::optional<MutationValue>& loaded,
                              MutationValue* out, QString* whyNot) {
    const QString trimmed = text.trimmed();
    const Kind like = loaded ? loaded->kind() : Kind::Null;
    switch (like) {
        case Kind::Str:
            *out = str(text);
            return true;
        case Kind::I64: {
            bool ok = false;
            const qint64 i = trimmed.toLongLong(&ok);
            if (!ok) {
                *whyNot = QStringLiteral("this field holds a whole number; “%1” is not one").arg(text);
                return false;
            }
            *out = i64(i);
            return true;
        }
        case Kind::F64: {
            bool ok = false;
            const double d = trimmed.toDouble(&ok);
            if (!ok) {
                *whyNot = QStringLiteral("this field holds a number; “%1” is not one").arg(text);
                return false;
            }
            *out = f64(d);
            return true;
        }
        case Kind::Bool: {
            const QString t = trimmed.toLower();
            if (t == QStringLiteral("true") || t == QStringLiteral("yes") ||
                t == QStringLiteral("1")) {
                *out = boolean(true);
                return true;
            }
            if (t == QStringLiteral("false") || t == QStringLiteral("no") ||
                t == QStringLiteral("0")) {
                *out = boolean(false);
                return true;
            }
            *whyNot = QStringLiteral("this field holds true or false; “%1” is neither").arg(text);
            return false;
        }
        case Kind::Null: {
            // No type to preserve: read the text the way JSON would.
            bool ok = false;
            const qint64 i = trimmed.toLongLong(&ok);
            if (ok) {
                *out = i64(i);
                return true;
            }
            const double d = trimmed.toDouble(&ok);
            if (ok) {
                *out = f64(d);
                return true;
            }
            if (trimmed == QStringLiteral("true")) {
                *out = boolean(true);
                return true;
            }
            if (trimmed == QStringLiteral("false")) {
                *out = boolean(false);
                return true;
            }
            *out = str(text);
            return true;
        }
    }
    return false;
}

// --- EditableResult --------------------------------------------------------

std::optional<EditableResult> EditableResult::decode(const QJsonValue& v) {
    if (!v.isObject()) {
        return std::nullopt;
    }
    const QJsonObject o = v.toObject();
    EditableResult out;
    for (const QJsonValue& f : o.value(QStringLiteral("identity")).toArray()) {
        if (f.isString()) {
            out.identity << f.toString();
        }
    }
    if (out.identity.isEmpty()) {
        return std::nullopt;
    }
    for (const QJsonValue& f : o.value(QStringLiteral("guard")).toArray()) {
        if (f.isString()) {
            out.guardFields << f.toString();
        }
    }
    out.root = o.value(QStringLiteral("root")).toString();
    out.atomicBatch = o.value(QStringLiteral("atomic_batch")).toBool(false);
    return out;
}

bool EditableResult::address(const QJsonObject& envelope, Address* out,
                             QString* whyNot) const {
    out->key.clear();
    out->expect.clear();
    for (const QString& field : identity) {
        if (!envelope.contains(field)) {
            continue;
        }
        const auto value = MutationValue::decode(envelope.value(field));
        if (!value || value->kind() == MutationValue::Kind::Null) {
            continue;
        }
        out->key.append(FieldValue{field, *value});
    }
    if (out->key.isEmpty()) {
        *whyNot = QStringLiteral(
                      "this row carries none of the fields that identify a document "
                      "(%1), so there is nothing to address a write to")
                      .arg(identity.join(QStringLiteral(", ")));
        return false;
    }
    for (const QString& field : guardFields) {
        const auto value = envelope.contains(field)
                               ? MutationValue::decode(envelope.value(field))
                               : std::nullopt;
        if (!value || value->kind() == MutationValue::Kind::Null) {
            *whyNot = QStringLiteral(
                          "this document was loaded without `%1`, so an edit to it could "
                          "only be sent unguarded — and an unguarded write would overwrite "
                          "whatever the server holds now")
                          .arg(field);
            return false;
        }
        out->expect.append(FieldValue{field, *value});
    }
    QStringList parts;
    for (const FieldValue& fv : out->key) {
        parts << fv.field + QLatin1Char('=') + fv.value.display();
    }
    out->id = parts.join(QChar(0x01));
    return true;
}

// --- the batch -------------------------------------------------------------

namespace {

QJsonArray pair(const FieldValue& fv) {
    return QJsonArray{
        QJsonArray{QJsonObject{{QStringLiteral("Field"), fv.field}}},
        fv.value.abiJson(),
    };
}

QJsonArray pairs(const QVector<FieldValue>& fvs) {
    QJsonArray out;
    for (const FieldValue& fv : fvs) {
        out.append(pair(fv));
    }
    return out;
}

}  // namespace

QJsonObject DocumentMutation::abiJson() const {
    QJsonObject body;
    body.insert(QStringLiteral("path"), QJsonArray::fromStringList(path));
    body.insert(QStringLiteral("key"), pairs(key));
    body.insert(QStringLiteral("expect"), pairs(expect));
    if (isDelete) {
        return QJsonObject{{QStringLiteral("Delete"), body}};
    }
    body.insert(QStringLiteral("sets"), pairs(sets));
    return QJsonObject{{QStringLiteral("Update"), body}};
}

QString mutationBatchJson(const QVector<DocumentMutation>& mutations) {
    QJsonArray list;
    for (const DocumentMutation& m : mutations) {
        list.append(m.abiJson());
    }
    return QString::fromUtf8(
        QJsonDocument(QJsonObject{{QStringLiteral("mutations"), list}})
            .toJson(QJsonDocument::Compact));
}

QJsonObject DocumentAddress::abiJson() const {
    return QJsonObject{{QStringLiteral("key"), pairs(key)}};
}

QString documentAddressBatchJson(const QVector<DocumentAddress>& addresses) {
    QJsonArray list;
    for (const DocumentAddress& a : addresses) {
        list.append(a.abiJson());
    }
    return QString::fromUtf8(
        QJsonDocument(QJsonObject{{QStringLiteral("documents"), list}})
            .toJson(QJsonDocument::Compact));
}

// --- the re-read -----------------------------------------------------------

ServerValue ServerValue::decode(const QJsonValue& v, bool present) {
    ServerValue out;
    if (!present || v.isUndefined()) {
        out.kind = Kind::Missing;
        return out;
    }
    if (auto value = MutationValue::decode(v)) {
        out.kind = Kind::Value;
        out.value = *value;
        return out;
    }
    out.kind = Kind::Nested;
    if (v.isArray()) {
        out.nestedWhat = QStringLiteral("an array");
    } else if (v.isObject()) {
        out.nestedWhat = QStringLiteral("an object");
    } else {
        out.nestedWhat = QStringLiteral("a value this view cannot show");
    }
    return out;
}

QString ServerValue::display() const {
    switch (kind) {
        case Kind::Value: return value.display();
        case Kind::Nested: return nestedWhat;
        case Kind::Missing: return QStringLiteral("—");
    }
    return QString();
}

std::optional<MutationValue> ServerValue::mutationValue() const {
    if (kind == Kind::Value) {
        return value;
    }
    return std::nullopt;
}

bool ServerDocument::decodeAll(const QString& json, QVector<ServerDocument>* out,
                               QString* whyNot) {
    const QJsonDocument doc = QJsonDocument::fromJson(json.toUtf8());
    if (!doc.isObject() ||
        !doc.object().value(QStringLiteral("documents")).isArray()) {
        *whyNot = QStringLiteral("the re-read was not a document list: %1").arg(json);
        return false;
    }
    out->clear();
    const QJsonArray documents =
        doc.object().value(QStringLiteral("documents")).toArray();
    for (const QJsonValue& d : documents) {
        const QJsonObject o = d.toObject();
        ServerDocument sd;
        sd.found = o.value(QStringLiteral("found")).toBool(false);
        const QJsonValue err = o.value(QStringLiteral("error"));
        if (err.isString()) {
            sd.error = err.toString();
        }
        const QJsonObject envelope = o.value(QStringLiteral("envelope")).toObject();
        for (auto it = envelope.begin(); it != envelope.end(); ++it) {
            sd.envelope.insert(it.key(), ServerValue::decode(it.value(), true));
        }
        const QJsonObject fields = o.value(QStringLiteral("fields")).toObject();
        for (auto it = fields.begin(); it != fields.end(); ++it) {
            sd.fields.insert(it.key(), ServerValue::decode(it.value(), true));
        }
        out->append(sd);
    }
    return true;
}

// --- the report ------------------------------------------------------------

bool MutationReport::decode(const QString& json, MutationReport* out,
                            QString* whyNot) {
    const QJsonDocument doc = QJsonDocument::fromJson(json.toUtf8());
    if (!doc.isObject()) {
        *whyNot = QStringLiteral("the mutation report was not an object: %1").arg(json);
        return false;
    }
    const QJsonObject o = doc.object();
    *out = MutationReport{};
    for (const QJsonValue& r : o.value(QStringLiteral("rows")).toArray()) {
        const QJsonObject ro = r.toObject();
        MutationRow row;
        row.op = ro.value(QStringLiteral("op")).toString(QStringLiteral("?"));
        row.index = ro.value(QStringLiteral("_index")).toString();
        row.documentId = ro.value(QStringLiteral("_id")).toString();
        row.routing = ro.value(QStringLiteral("_routing")).toString();
        const QString outcome = ro.value(QStringLiteral("outcome")).toString();
        if (outcome == QStringLiteral("applied")) {
            row.outcome = MutationRow::Outcome::Applied;
        } else if (outcome == QStringLiteral("not attempted")) {
            row.outcome = MutationRow::Outcome::NotAttempted;
        } else {
            row.outcome = MutationRow::Outcome::Failed;
        }
        const QJsonValue seq = ro.value(QStringLiteral("_seq_no"));
        if (seq.isDouble()) {
            row.seqNo = seq.toVariant().toLongLong();
        }
        const QJsonValue term = ro.value(QStringLiteral("_primary_term"));
        if (term.isDouble()) {
            row.primaryTerm = term.toVariant().toLongLong();
        }
        row.conflict = ro.value(QStringLiteral("conflict")).toBool(false);
        row.errorCode = ro.value(QStringLiteral("error_code")).toString();
        row.error = ro.value(QStringLiteral("error")).toString();
        row.forcedRefresh = ro.value(QStringLiteral("forced_refresh")).toBool(false);
        out->rows.append(row);
    }
    for (const QJsonValue& n : o.value(QStringLiteral("notices")).toArray()) {
        const QJsonObject no = n.toObject();
        const QString message = no.value(QStringLiteral("message")).toString();
        if (message.isEmpty()) {
            continue;
        }
        MutationNotice notice;
        notice.severity =
            no.value(QStringLiteral("severity")).toString(QStringLiteral("info"));
        notice.code = no.value(QStringLiteral("code")).toString();
        notice.message = message;
        out->notices.append(notice);
    }
    const QJsonObject summary = o.value(QStringLiteral("summary")).toObject();
    out->applied = summary.value(QStringLiteral("applied")).toInt();
    out->failed = summary.value(QStringLiteral("failed")).toInt();
    out->notAttempted = summary.value(QStringLiteral("not_attempted")).toInt();
    out->conflicts = summary.value(QStringLiteral("conflicts")).toInt();
    return true;
}

}  // namespace dg
