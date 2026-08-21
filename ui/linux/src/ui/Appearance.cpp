#include "Appearance.hpp"

#include "Theme.hpp"

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
        dg::applyStyleSheet(isDark());
        emit effectivePaletteChanged(QApplication::palette(), isDark());
        return;
    }
    const QPalette palette = mode == Mode::Light   ? dg::lightPalette()
                             : mode == Mode::Dark  ? dg::darkPalette()
                                                   : systemPalette_;
    forced_ = mode != Mode::System;
    applying_ = true;
    QApplication::setPalette(palette);
    applying_ = false;
    dg::applyStyleSheet(isDark(palette));
    emit effectivePaletteChanged(palette, isDark(palette));
}

bool Appearance::eventFilter(QObject* watched, QEvent* event) {
    if (watched == qApp &&
        event->type() == QEvent::ApplicationPaletteChange && !applying_ &&
        !forced_) {
        systemPalette_ = QApplication::palette();
        dg::applyStyleSheet(isDark(systemPalette_));
        emit effectivePaletteChanged(systemPalette_, isDark(systemPalette_));
    }
    return QObject::eventFilter(watched, event);
}
