// Appearance.hpp — follow system / force light / force dark, applied through
// the application palette. Follow-system is the default and sets NO palette at
// all: the platform theme keeps driving, live, which is what a well-behaved
// Qt app does.
//
// effectivePaletteChanged is the hook for anything that resolves light/dark
// asset variants (engine icons): react to the signal, never to the stored
// setting — in follow-system the palette moves without the setting changing.

#ifndef DATAGREP_APPEARANCE_HPP
#define DATAGREP_APPEARANCE_HPP

#include <QObject>
#include <QPalette>

class Appearance : public QObject {
    Q_OBJECT

public:
    enum class Mode { System, Light, Dark };

    static Appearance& instance();

    static Mode mode();
    void setMode(Mode mode);

    // Applies the stored mode. Call once, after QApplication exists and
    // before any window shows.
    void applyStored();

    // The one truth for light-vs-dark, computed from the palette actually in
    // effect — right in every mode, including follow-system.
    static bool isDark(const QPalette& palette);
    bool isDark() const;

signals:
    // The effective palette changed: a mode switch, or the desktop theme
    // flipping while in follow-system.
    void effectivePaletteChanged(const QPalette& palette, bool dark);

protected:
    bool eventFilter(QObject* watched, QEvent* event) override;

private:
    Appearance();
    void apply(Mode mode);

    // The platform theme's palette, tracked while unforced. Forcing a palette
    // stops Qt delivering theme changes, so a theme flip that happens while
    // forced is only picked up on the next launch.
    QPalette systemPalette_;
    bool forced_ = false;
    bool applying_ = false;
};

#endif  // DATAGREP_APPEARANCE_HPP
