#include "SqlEditor.hpp"

#include <QEvent>
#include <QFont>
#include <QFontDatabase>
#include <QKeyEvent>
#include <QVector>

#ifdef HAVE_KSYNTAXHIGHLIGHTING
#include <QColor>
#include <QPalette>

#include <KSyntaxHighlighting/Definition>
#include <KSyntaxHighlighting/Repository>
#include <KSyntaxHighlighting/SyntaxHighlighter>
#include <KSyntaxHighlighting/Theme>
#else
#include "SqlHighlighter.hpp"
#endif

namespace {

// One statement span [start, end) in the buffer.
struct Span {
    int start;
    int end;
};

// Splits `sql` into top-level statement spans, treating ';' as a separator only
// when it is outside a string literal, quoted identifier, line comment or block
// comment. The separators themselves are not included in a span. Trailing
// whitespace-only spans are dropped by the caller.
QVector<Span> splitStatements(const QString& sql) {
    QVector<Span> spans;
    int spanStart = 0;
    bool inSingle = false;   // '…'
    bool inDouble = false;   // "…"
    bool inLine = false;     // -- …
    bool inBlock = false;    // /* … */
    const int n = sql.length();
    for (int i = 0; i < n; ++i) {
        const QChar c = sql.at(i);
        const QChar next = (i + 1 < n) ? sql.at(i + 1) : QChar();

        if (inLine) {
            if (c == QLatin1Char('\n')) inLine = false;
            continue;
        }
        if (inBlock) {
            if (c == QLatin1Char('*') && next == QLatin1Char('/')) {
                inBlock = false;
                ++i;
            }
            continue;
        }
        if (inSingle) {
            // '' is an escaped quote inside a single-quoted string.
            if (c == QLatin1Char('\'')) {
                if (next == QLatin1Char('\'')) {
                    ++i;
                } else {
                    inSingle = false;
                }
            }
            continue;
        }
        if (inDouble) {
            if (c == QLatin1Char('"')) {
                if (next == QLatin1Char('"')) {
                    ++i;
                } else {
                    inDouble = false;
                }
            }
            continue;
        }

        // Not currently inside anything.
        if (c == QLatin1Char('-') && next == QLatin1Char('-')) {
            inLine = true;
            ++i;
        } else if (c == QLatin1Char('/') && next == QLatin1Char('*')) {
            inBlock = true;
            ++i;
        } else if (c == QLatin1Char('\'')) {
            inSingle = true;
        } else if (c == QLatin1Char('"')) {
            inDouble = true;
        } else if (c == QLatin1Char(';')) {
            spans.push_back({spanStart, i});
            spanStart = i + 1;
        }
    }
    if (spanStart < n) {
        spans.push_back({spanStart, n});
    }
    return spans;
}

}  // namespace

SqlEditor::SqlEditor(QWidget* parent) : QPlainTextEdit(parent) {
    const QFont mono = QFontDatabase::systemFont(QFontDatabase::FixedFont);
    setFont(mono);
    setTabChangesFocus(false);
    setLineWrapMode(QPlainTextEdit::NoWrap);
    setPlaceholderText(QStringLiteral("SELECT …   (Ctrl+Return to run)"));

#ifdef HAVE_KSYNTAXHIGHLIGHTING
    // Preferred path: the maintained KSyntaxHighlighting engine. The Repository
    // owns the bundled syntax definitions and themes; construct the highlighter
    // against this editor's document and point it at the "SQL" definition.
    repository_ = new KSyntaxHighlighting::Repository();
    auto* ksh = new KSyntaxHighlighting::SyntaxHighlighter(document());
    ksh->setDefinition(repository_->definitionForName(QStringLiteral("SQL")));
    highlighter_ = ksh;
    applyTheme();
#else
    // Fallback path: the built-in basic highlighter (no external dependency).
    highlighter_ = new SqlHighlighter(document());
#endif
}

void SqlEditor::applyTheme() {
#ifdef HAVE_KSYNTAXHIGHLIGHTING
    if (repository_ == nullptr || applyingTheme_) {
        return;
    }
    auto* ksh = static_cast<KSyntaxHighlighting::SyntaxHighlighter*>(highlighter_);

    // Pick a bundled theme (light or dark) that matches the current palette, so
    // the highlight colours agree with the surrounding UI chrome.
    const KSyntaxHighlighting::Theme theme = repository_->themeForPalette(palette());
    ksh->setTheme(theme);

    if (theme.isValid()) {
        // Align the editor's own background / default text colour with the theme
        // so untokenised text and the caret line read correctly. Guard the
        // re-entrant PaletteChange this setPalette() triggers.
        applyingTheme_ = true;
        QPalette pal = palette();
        pal.setColor(QPalette::Base,
                     QColor(theme.editorColor(
                         KSyntaxHighlighting::Theme::BackgroundColor)));
        pal.setColor(
            QPalette::Text,
            QColor(theme.textColor(KSyntaxHighlighting::Theme::Normal)));
        setPalette(pal);
        applyingTheme_ = false;
    }

    ksh->rehighlight();
#endif
}

void SqlEditor::changeEvent(QEvent* event) {
    QPlainTextEdit::changeEvent(event);
#ifdef HAVE_KSYNTAXHIGHLIGHTING
    if (event != nullptr && event->type() == QEvent::PaletteChange) {
        applyTheme();
    }
#endif
}

QString SqlEditor::allText() const { return toPlainText().trimmed(); }

QString SqlEditor::statementUnderCursor() const {
    const QString sql = toPlainText();
    const int pos = textCursor().position();
    const QVector<Span> spans = splitStatements(sql);
    for (const Span& s : spans) {
        // The cursor belongs to the span it sits within; a cursor exactly on a
        // separator boundary attaches to the statement that just ended.
        if (pos >= s.start && pos <= s.end) {
            const QString stmt = sql.mid(s.start, s.end - s.start).trimmed();
            if (!stmt.isEmpty()) {
                return stmt;
            }
        }
    }
    // No non-empty statement contained the cursor: fall back to the whole buffer.
    return sql.trimmed();
}

void SqlEditor::keyPressEvent(QKeyEvent* event) {
    const bool runChord =
        (event->key() == Qt::Key_Return || event->key() == Qt::Key_Enter) &&
        (event->modifiers() & (Qt::ControlModifier | Qt::MetaModifier));
    if (runChord) {
        emit runRequested();
        event->accept();
        return;
    }
    QPlainTextEdit::keyPressEvent(event);
}
