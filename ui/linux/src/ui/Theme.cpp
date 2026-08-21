#include "Theme.hpp"
#include <QApplication>
#include <QFile>
#include <QStyleFactory>

namespace dg {
namespace {

// Light column = the sheet's original literals; dark harmonises with darkPalette().
struct ThemeColor {
    const char* token;
    const char* light;
    const char* dark;
};

constexpr ThemeColor kColors[] = {
    {"@surface@", "#ffffff", "#3e4042"},
    {"@surfaceHover@", "#f2f4f7", "#47494c"},
    {"@surfacePressed@", "#e6e9ee", "#4d5054"},
    {"@border@", "#c8cdd5", "#55585c"},
    {"@accent@", "#3584e4", "#2a82da"},
    {"@accentBorder@", "#2b70c4", "#2470bd"},
    {"@accentHover@", "#2f79d5", "#3b8de0"},
    {"@accentPressed@", "#2a6cc0", "#2265a8"},
    {"@disabledSurface@", "#f2f3f5", "#37393b"},
    {"@disabledBorder@", "#dfe2e7", "#44464a"},
    {"@disabledText@", "#a0a6b0", "#7f7f7f"},
    {"@hoverTint@", "#e8ebef", "#45484b"},
    {"@pressedTint@", "#dde1e7", "#4d5054"},
    {"@tabHover@", "#eceef2", "#404346"},
    {"@headerBg@", "#f4f5f7", "#353535"},
    {"@headerBorderRight@", "#e0e3e8", "#2a2a2c"},
    {"@headerBorderBottom@", "#d3d7dd", "#232325"},
    {"@gridline@", "#e3e6ea", "#3a3d40"},
    {"@viewBg@", "#ffffff", "#2a2a2a"},
    {"@viewBorder@", "#d5d9df", "#45484c"},
    {"@scrollHandle@", "#c3c9d1", "#5a5e64"},
    {"@scrollHandleHover@", "#a9b1bb", "#6d7178"},
    {"@separator@", "#e3e6ea", "#4a4d50"},
    {"@mutedText@", "#5c6470", "#a8adb5"},
    {"@tooltipText@", "#23272e", "#e8e8e8"},
};

}  // namespace

QPalette lightPalette() {
    QPalette p;
    p.setColor(QPalette::Window, QColor(0xf4, 0xf5, 0xf7));
    p.setColor(QPalette::WindowText, QColor(0x23, 0x27, 0x2e));
    p.setColor(QPalette::Base, QColor(0xff, 0xff, 0xff));
    p.setColor(QPalette::AlternateBase, QColor(0xf7, 0xf8, 0xfa));
    p.setColor(QPalette::Text, QColor(0x23, 0x27, 0x2e));
    p.setColor(QPalette::PlaceholderText, QColor(0x90, 0x99, 0xa6));
    p.setColor(QPalette::Button, QColor(0xf4, 0xf5, 0xf7));
    p.setColor(QPalette::ButtonText, QColor(0x23, 0x27, 0x2e));
    p.setColor(QPalette::BrightText, QColor(0xff, 0xff, 0xff));
    p.setColor(QPalette::Highlight, QColor(0x35, 0x84, 0xe4));
    p.setColor(QPalette::HighlightedText, QColor(0xff, 0xff, 0xff));
    p.setColor(QPalette::Link, QColor(0x1a, 0x6f, 0xc4));
    p.setColor(QPalette::ToolTipBase, QColor(0xff, 0xff, 0xff));
    p.setColor(QPalette::ToolTipText, QColor(0x23, 0x27, 0x2e));

    const QColor disabledText(0xa0, 0xa6, 0xb0);
    p.setColor(QPalette::Disabled, QPalette::WindowText, disabledText);
    p.setColor(QPalette::Disabled, QPalette::Text, disabledText);
    p.setColor(QPalette::Disabled, QPalette::ButtonText, disabledText);
    p.setColor(QPalette::Disabled, QPalette::Highlight,
               QColor(0xc9, 0xcd, 0xd4));
    p.setColor(QPalette::Disabled, QPalette::HighlightedText,
               QColor(0xff, 0xff, 0xff));
    return p;
}

QPalette darkPalette() {
    QPalette p(QColor(0x35, 0x35, 0x35));
    p.setColor(QPalette::Window, QColor(0x35, 0x35, 0x35));
    p.setColor(QPalette::WindowText, Qt::white);
    p.setColor(QPalette::Base, QColor(0x2A, 0x2A, 0x2A));
    p.setColor(QPalette::AlternateBase, QColor(0x42, 0x42, 0x42));
    p.setColor(QPalette::ToolTipBase, QColor(0x3e, 0x40, 0x42));
    p.setColor(QPalette::ToolTipText, QColor(0xe8, 0xe8, 0xe8));
    p.setColor(QPalette::Text, Qt::white);
    p.setColor(QPalette::PlaceholderText, QColor(0x8C, 0x8C, 0x8C));
    p.setColor(QPalette::Button, QColor(0x35, 0x35, 0x35));
    p.setColor(QPalette::ButtonText, Qt::white);
    p.setColor(QPalette::BrightText, QColor(0xFF, 0x45, 0x45));
    p.setColor(QPalette::Link, QColor(0x2A, 0x82, 0xDA));
    p.setColor(QPalette::Highlight, QColor(0x2A, 0x82, 0xDA));
    p.setColor(QPalette::HighlightedText, Qt::black);
    p.setColor(QPalette::Disabled, QPalette::WindowText, QColor(0x7F, 0x7F, 0x7F));
    p.setColor(QPalette::Disabled, QPalette::Text, QColor(0x7F, 0x7F, 0x7F));
    p.setColor(QPalette::Disabled, QPalette::ButtonText, QColor(0x7F, 0x7F, 0x7F));
    return p;
}

void applyStyleSheet(bool dark) {
    QFile qss(QStringLiteral(":/style/datagrep.qss"));
    if (!qss.open(QIODevice::ReadOnly)) {
        return;
    }
    QString sheet = QString::fromUtf8(qss.readAll());
    for (const ThemeColor& c : kColors) {
        sheet.replace(QLatin1String(c.token),
                      QLatin1String(dark ? c.dark : c.light));
    }
    qApp->setStyleSheet(sheet);
}

void applyTheme() {
    if (!qEnvironmentVariableIsSet("QT_STYLE_OVERRIDE")) {
        QApplication::setStyle(QStyleFactory::create(QStringLiteral("Fusion")));
    }
    QApplication::setPalette(lightPalette());
    applyStyleSheet(false);
}

}  // namespace dg
