// Mutation.hpp — the editing wire types: what a result says about editing it,
// one staged value, the batch datagrep_mutate parses, and the report and
// re-read payloads it returns.
//
// Linux counterpart of DatagrepKit.Mutation. The JSON here is serde's
// externally-tagged spelling and must stay byte-equivalent to what the macOS
// encoders build — the engine's own tests pin that wire format from the other
// side. Guard field names (_seq_no, _primary_term) never appear in this file:
// they arrive in the status' `editable` block, so the UI stays engine-neutral.

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

// One value crossing the mutation ABI, in the engine's own `Value` spelling.
// Deliberately a small set: these are the types a grid cell can be typed into.
// An object or an array is edited as a document, not as a cell.
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

    // serde's externally-tagged form: {"Str":"x"}, {"I64":42}, and a bare
    // "Null" string for the unit variant.
    QJsonValue abiJson() const;

    // What this value looks like in a grid cell / an error message.
    QString display() const;

    bool operator==(const MutationValue& other) const;
    bool operator!=(const MutationValue& other) const { return !(*this == other); }

    // Reads one value out of parsed JSON. Objects and arrays return nullopt:
    // they are values this type deliberately cannot carry, not values it
    // should flatten.
    static std::optional<MutationValue> decode(const QJsonValue& v);

    // Decodes a bare JSON fragment ("42", "\"x\"", "null") — the shape
    // datagrep_rows_cell_detail_json returns for a scalar cell, which
    // QJsonDocument refuses as a document.
    static std::optional<MutationValue> decodeFragment(const QString& json);

    // The typed text, coerced to the type the loaded value had. A field that
    // came back as a number goes back as a number — retyping it as a string
    // would silently rewrite the field's type on a server that types its
    // fields, so the coercion is refused here where the sentence can still
    // name the value. A field loaded as NULL (or with no loaded value) is read
    // the way JSON would read it.
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

// The `editable` block of datagrep_query_status_json. Absent means no edit may
// be offered at all — which is also what a connection that has not answered
// yet reports, because "we have not asked" and "yes" are different facts.
struct EditableResult {
    // Fields that name exactly one row. They become the mutation's `key`.
    QStringList identity;
    // Fields a write compares against before applying. They become `expect`,
    // carrying the values that were LOADED — the whole compare-and-swap.
    QStringList guardFields;
    // The field the grid's columns are projected from; empty = none.
    QString root;
    // False means a failing batch can leave the mutations before it applied.
    // The confirmation says so BEFORE the click.
    bool atomicBatch = false;

    // nullopt for anything that is not a usable identity: a malformed or
    // half-present block reads as "not editable", never as a partial yes.
    static std::optional<EditableResult> decode(const QJsonValue& v);

    struct Address {
        // Identity values joined field=value with \x01 — stable across a
        // re-render, unlike a row index. Two documents are the same one
        // exactly when the engine would address them identically.
        QString id;
        QVector<FieldValue> key;
        QVector<FieldValue> expect;
    };

    // The address one row's write needs, read out of that row's envelope.
    // Refuses (returns false, fills whyNot) when nothing names the document or
    // a guard field is missing — a write could then only go unguarded, which
    // the engine refuses anyway; the sentence is worth more here. An identity
    // field simply not on this row is left out rather than sent as null.
    bool address(const QJsonObject& envelope, Address* out, QString* whyNot) const;
};

// One document's write, addressed the way the engine addresses it. `key` says
// WHICH document, `expect` says WHICH VERSION of it — nothing here ever leaves
// `expect` off to "make it work".
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

// One document to re-read, addressed exactly as its write was. The read half
// of a version conflict — nothing about it retries a write.
struct DocumentAddress {
    QVector<FieldValue> key;
    QJsonObject abiJson() const;
};

// The address list datagrep_reread_documents parses.
QString documentAddressBatchJson(const QVector<DocumentAddress>& addresses);

// One field of a document as the server holds it now. `Nested` rather than a
// flattened value: a field that became an object or array cannot be compared
// against a typed cell, and saying which it is beats a blank read as "empty".
struct ServerValue {
    enum class Kind { Value, Nested, Missing };
    Kind kind = Kind::Missing;
    MutationValue value;  // meaningful only when kind == Value
    QString nestedWhat;   // "an array" / "an object"

    static ServerValue decode(const QJsonValue& v, bool present);
    QString display() const;
    std::optional<MutationValue> mutationValue() const;
};

// What one re-read found. found == false with no error means the document is
// simply gone — a resolution in itself, with nothing to rebase onto.
struct ServerDocument {
    bool found = false;
    QString error;
    // Outside the projected root: which document, and the FRESH guard values a
    // rebase re-guards against.
    QHash<QString, ServerValue> envelope;
    // The document itself, at its root.
    QHash<QString, ServerValue> fields;

    static bool decodeAll(const QString& json, QVector<ServerDocument>* out,
                          QString* whyNot);
};

// What happened to one document. A conflict is a row here, not an error: the
// batch still returns a report, and the conflict is a state the UI shows.
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
    // The server escalated a wait_for refresh to an immediate one — a load the
    // cluster paid for this save.
    bool forcedRefresh = false;
};

// A non-fatal message the engine sent along with the batch. Shown, never
// swallowed.
struct MutationNotice {
    QString severity;
    QString code;
    QString message;
    bool isWarning() const { return severity == QStringLiteral("warning"); }
};

// The whole report datagrep_mutate returns.
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
