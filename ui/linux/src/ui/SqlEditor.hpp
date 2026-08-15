// SqlEditor.hpp — the SQL editing pane.
//
// A QPlainTextEdit with the placeholder SqlHighlighter attached. The widget talks
// to its highlighter only through the QSyntaxHighlighter base, so swapping in
// KSyntaxHighlighting (preferred, MIT) or a QScintilla widget (GPLv3 — see
// SqlHighlighter.hpp) is a localised change.
//
// statementUnderCursor() implements "run the statement under the cursor": the
// text is split into statements on ';' boundaries that are NOT inside a string
// literal or a comment, and the statement containing the cursor is returned.
// Exactly that substring is what MainWindow passes to datagrep_query_run.

#ifndef DATAGREP_SQL_EDITOR_HPP
#define DATAGREP_SQL_EDITOR_HPP

#include <QPlainTextEdit>

class SqlHighlighter;

class SqlEditor : public QPlainTextEdit {
    Q_OBJECT

public:
    explicit SqlEditor(QWidget* parent = nullptr);

    // The whole buffer, trimmed.
    QString allText() const;

    // The statement the cursor sits in, split on top-level ';' (semicolons inside
    // '…' strings, "…" identifiers, -- line comments and /* … */ block comments
    // are ignored). Falls back to the whole buffer if splitting finds nothing.
    QString statementUnderCursor() const;

signals:
    // Emitted on Ctrl+Return / Cmd+Return: "run this".
    void runRequested();

protected:
    void keyPressEvent(QKeyEvent* event) override;

private:
    SqlHighlighter* highlighter_;
};

#endif  // DATAGREP_SQL_EDITOR_HPP
