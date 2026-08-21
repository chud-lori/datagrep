// Mutation.hpp — the editing wire types.

// Wire JSON must stay byte-equivalent to the macOS encoders; the engine's tests pin it.
#ifndef DATAGREP_MUTATION_HPP
#define DATAGREP_MUTATION_HPP

#include <QHash>
#include <QJsonObject>
#include <QJsonValue>
#include <QString>
#include <QStringList>
#include <QVector>

#include <cstdint>
#include <optional>

namespace dg {

class MutationValue {
public:
    enum class Kind { Str, I64, F64, Bool, Null };

    MutationValue() : kind_(Kind::Null) {}
    static MutationValue str(const QString& s);
    static MutationValue i64(qint64 i);
    static MutationValue f64(double d);
    static MutationValue boolean(bool b);
    static MutationValue null();

    Kind kind() const { return kind_; }

    QJsonValue abiJson() const;

    QString display() const;

    bool operator==(const MutationValue& other) const;
    bool operator!=(const MutationValue& other) const { return !(*this == other); }

    static std::optional<MutationValue> decode(const QJsonValue& v);

    static std::optional<MutationValue> decodeFragment(const QString& json);

    // Coerces to the loaded value's type — a string would silently retype the field.
    static bool typedLike(const QString& text,
                          const std::optional<MutationValue>& loaded,
                          MutationValue* out, QString* whyNot);

private:
    Kind kind_;
    QString s_;
    qint64 i_ = 0;
    double d_ = 0;
    bool b_ = false;
};

// One field paired with its value — the wire's `(FieldPath, Value)`.
struct FieldValue {
    QString field;
    MutationValue value;
};

struct EditableResult {
    // Fields that name exactly one row. They become the mutation's `key`.
    QStringList identity;
    // `expect` carries the values that were LOADED — the compare-and-swap guard.
    QStringList guardFields;
    // The field the grid's columns are projected from; empty = none.
    QString root;
    // False means a failing batch can leave the mutations before it applied.
    bool atomicBatch = false;

    static std::optional<EditableResult> decode(const QJsonValue& v);

    struct Address {
        QString id;
        QVector<FieldValue> key;
        QVector<FieldValue> expect;
    };

    bool address(const QJsonObject& envelope, Address* out, QString* whyNot) const;
};

struct DocumentMutation {
    QStringList path;  // where a NEW document would go; empty — nothing here inserts
    QVector<FieldValue> key;
    QVector<FieldValue> expect;
    QVector<FieldValue> sets;  // empty for a delete
    bool isDelete = false;

    QJsonObject abiJson() const;
};

// The MutationBatch blob datagrep_mutate parses.
QString mutationBatchJson(const QVector<DocumentMutation>& mutations);

struct DocumentAddress {
    QVector<FieldValue> key;
    QJsonObject abiJson() const;
};

// The address list datagrep_reread_documents parses.
QString documentAddressBatchJson(const QVector<DocumentAddress>& addresses);

struct ServerValue {
    enum class Kind { Value, Nested, Missing };
    Kind kind = Kind::Missing;
    MutationValue value;  // meaningful only when kind == Value
    QString nestedWhat;   // "an array" / "an object"

    static ServerValue decode(const QJsonValue& v, bool present);
    QString display() const;
    std::optional<MutationValue> mutationValue() const;
};

struct ServerDocument {
    bool found = false;
    QString error;
    QHash<QString, ServerValue> envelope;
    QHash<QString, ServerValue> fields;

    static bool decodeAll(const QString& json, QVector<ServerDocument>* out,
                          QString* whyNot);
};

struct MutationRow {
    enum class Outcome { Applied, Failed, NotAttempted };

    QString op;
    QString index;
    QString documentId;
    QString routing;
    Outcome outcome = Outcome::Failed;
    std::optional<qint64> seqNo;
    std::optional<qint64> primaryTerm;
    bool conflict = false;
    QString errorCode;
    QString error;
    // The server escalated a wait_for refresh to an immediate one.
    bool forcedRefresh = false;
};

// A non-fatal message the engine sent along with the batch. Shown, never swallowed.
struct MutationNotice {
    QString severity;
    QString code;
    QString message;
    bool isWarning() const { return severity == QStringLiteral("warning"); }
};

// The report datagrep_mutate returns.
struct MutationReport {
    QVector<MutationRow> rows;
    QVector<MutationNotice> notices;
    int applied = 0;
    int failed = 0;
    int notAttempted = 0;
    int conflicts = 0;

    bool isClean() const { return failed == 0 && notAttempted == 0; }

    static bool decode(const QString& json, MutationReport* out, QString* whyNot);
};

}  // namespace dg

#endif  // DATAGREP_MUTATION_HPP
