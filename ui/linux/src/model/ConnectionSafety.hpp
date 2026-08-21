// ConnectionSafety.hpp — the safety facts about one connection, and the two
// helpers that turn them into behaviour.
//
// This is the Linux counterpart of the macOS ConnectionSafety type: the
// connection list, the marked banner and the run path must all answer "how
// dangerous is this connection" the same way, so the facts are resolved into
// one struct instead of each surface re-reading the profile JSON.
//
// Two of these facts are ENFORCED here rather than merely displayed:
//
//  * confirmWrites — the profile asked to be prompted before every write. The
//    run path classifies the statement with isWriteStatement() and puts up a
//    modal before sending. (The engine has no notion of this setting; it is a
//    UI promise, so the UI must keep it.)
//  * color — the user marked the connection. datagrep does not know what the
//    colour means — that is the point of letting the user choose it — but a
//    marked connection must be unmissable, so the colour becomes a filled
//    banner and a swatch in the list, never just a stored string.
//
// isWriteStatement() is a fat-finger guardrail, not an adversary defence — the
// same words, in the same spirit, as the macOS classifier it mirrors
// (DatagrepKit.SQLBlocks.isWriteStatement). Real refusal of writes on a
// read-only profile belongs to the engine and already lives there.

#ifndef DATAGREP_CONNECTION_SAFETY_HPP
#define DATAGREP_CONNECTION_SAFETY_HPP

#include <QColor>
#include <QString>

#include <optional>

namespace dg {

// The safety-relevant slice of one profile, as parsed from profiles_list JSON.
struct ConnectionSafety {
    QString name;
    QString color;          // one of the marker palette names; empty = unmarked
    QString env;            // legacy dev/staging/prod tag, still in the store
    bool readOnly = false;
    bool confirmWrites = false;

    bool isMarked() const { return !color.isEmpty(); }
};

// The marker palette, by name. The names are the contract — they are what the
// profile store carries and what the macOS app shows — so both UIs accept
// exactly the same set and an unknown name renders as unmarked rather than
// guessing. Kept in ONE place for the same reason the macOS app has exactly one
// ConnectionColor: the colour is a recognition cue, and a cue that renders
// differently in two surfaces stops being one.
std::optional<QColor> connectionColor(const QString& name);

// True when the statement's first word is a write/DDL verb. Leading `--`
// comment lines are skipped first (block directives live there), then the first
// run of letters is compared, case-insensitively, against the same verb list
// the macOS classifier uses. Anything unrecognised counts as NOT a write — the
// prompt exists to catch the obvious fat-finger, not to fence the engine.
bool isWriteStatement(const QString& sql);

// The first word of the statement, uppercased, for a prompt that says what it
// is prompting about ("Run a DELETE against …?"). Falls back to "statement".
QString statementVerb(const QString& sql);

}  // namespace dg

#endif  // DATAGREP_CONNECTION_SAFETY_HPP
