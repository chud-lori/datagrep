#include "Appearance.hpp"

#include <QApplication>
#include <QEvent>
#include <QSettings>

namespace {

QString settingsKey() { return QStringLiteral("appearance"); }

QString toString(Appearance::Mode mode) {
    switch (mode) {
        case Appearance::Mode::Light:
            return QStringLiteral("light");
        case Appearance::Mode::Dark:
            return QStringLiteral("dark");
        case Appearance::Mode::System:
            break;
    }
    return QStringLiteral("system");
}

Appearance::Mode fromString(const QString& raw) {
    if (raw == QStringLiteral("light")) {
        return Appearance::Mode::Light;
    }
    if (raw == QStringLiteral("dark")) {
        return Appearance::Mode::Dark;
    }
    return Appearance::Mode::System;
}

QPalette lightPalette() {
    QPalette p(QColor(0xEF, 0xEF, 0xEF));
    p.setColor(QPalette::Window, QColor(0xEF, 0xEF, 0xEF));
    p.setColor(QPalette::WindowText, Qt::black);
    p.setColor(QPalette::Base, Qt::white);
    p.setColor(QPalette::AlternateBase, QColor(0xF5, 0xF5, 0xF5));
    p.setColor(QPalette::ToolTipBase, QColor(0xFF, 0xFF, 0xDC));
    p.setColor(QPalette::ToolTipText, Qt::black);
    p.setColor(QPalette::Text, Qt::black);
    p.setColor(QPalette::PlaceholderText, QColor(0x80, 0x80, 0x80));
    p.setColor(QPalette::Button, QColor(0xEF, 0xEF, 0xEF));
    p.setColor(QPalette::ButtonText, Qt::black);
    p.setColor(QPalette::BrightText, Qt::white);
    p.setColor(QPalette::Link, QColor(0x1B, 0x6A, 0xCB));
    p.setColor(QPalette::Highlight, QColor(0x30, 0x8C, 0xC6));
    p.setColor(QPalette::HighlightedText, Qt::white);
    p.setColor(QPalette::Disabled, QPalette::WindowText, QColor(0x78, 0x78, 0x78));
    p.setColor(QPalette::Disabled, QPalette::Text, QColor(0x78, 0x78, 0x78));
    p.setColor(QPalette::Disabled, QPalette::ButtonText, QColor(0x78, 0x78, 0x78));
    return p;
}

QPalette darkPalette() {
    QPalette p(QColor(0x35, 0x35, 0x35));
    p.setColor(QPalette::Window, QColor(0x35, 0x35, 0x35));
    p.setColor(QPalette::WindowText, Qt::white);
    p.setColor(QPalette::Base, QColor(0x2A, 0x2A, 0x2A));
    p.setColor(QPalette::AlternateBase, QColor(0x42, 0x42, 0x42));
    p.setColor(QPalette::ToolTipBase, QColor(0x35, 0x35, 0x35));
    p.setColor(QPalette::ToolTipText, Qt::white);
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

}  // namespace

Appearance& Appearance::instance() {
    static Appearance appearance;
    return appearance;
}

Appearance::Appearance() : systemPalette_(QApplication::palette()) {
    qApp->installEventFilter(this);
}

Appearance::Mode Appearance::mode() {
    return fromString(QSettings().value(settingsKey()).toString());
}

void Appearance::setMode(Mode mode) {
    QSettings().setValue(settingsKey(), toString(mode));
    apply(mode);
}

void Appearance::applyStored() {
    apply(mode());
}

bool Appearance::isDark(const QPalette& palette) {
    return palette.color(QPalette::Window).lightness() <
           palette.color(QPalette::WindowText).lightness();
}

bool Appearance::isDark() const {
    return isDark(QApplication::palette());
}

void Appearance::apply(Mode mode) {
    if (mode == Mode::System && !forced_) {
        emit effectivePaletteChanged(QApplication::palette(), isDark());
        return;
    }
    const QPalette palette = mode == Mode::Light   ? lightPalette()
                             : mode == Mode::Dark  ? darkPalette()
                                                   : systemPalette_;
    forced_ = mode != Mode::System;
    applying_ = true;
    QApplication::setPalette(palette);
    applying_ = false;
    emit effectivePaletteChanged(palette, isDark(palette));
}

bool Appearance::eventFilter(QObject* watched, QEvent* event) {
    if (watched == qApp &&
        event->type() == QEvent::ApplicationPaletteChange && !applying_ &&
        !forced_) {
        systemPalette_ = QApplication::palette();
        emit effectivePaletteChanged(systemPalette_, isDark(systemPalette_));
    }
    return QObject::eventFilter(watched, event);
}
