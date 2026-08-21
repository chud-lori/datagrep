#include "Theme.hpp"
#include <QApplication>
#include <QFile>
#include <QPalette>
#include <QStyleFactory>

namespace dg {
namespace {

// Light-only: dark detection needs QStyleHints::colorScheme (Qt 6.5); the
// deployment floor is 6.2. Colors match the literals in datagrep.qss.
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

}  // namespace

void applyTheme(QApplication& app) {
    if (!qEnvironmentVariableIsSet("QT_STYLE_OVERRIDE")) {
        QApplication::setStyle(QStyleFactory::create(QStringLiteral("Fusion")));
    }
    QApplication::setPalette(lightPalette());

    QFile qss(QStringLiteral(":/style/datagrep.qss"));
    if (qss.open(QIODevice::ReadOnly)) {
        app.setStyleSheet(QString::fromUtf8(qss.readAll()));
    }
}

}  // namespace dg
