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

    // Call once, after QApplication exists and before any window shows.
    void applyStored();

    // The one truth for light-vs-dark: the palette actually in effect.
    static bool isDark(const QPalette& palette);
    bool isDark() const;

signals:
    void effectivePaletteChanged(const QPalette& palette, bool dark);

protected:
    bool eventFilter(QObject* watched, QEvent* event) override;

private:
    Appearance();
    void apply(Mode mode);

    // Forcing a palette stops Qt delivering theme changes until the next launch.
    QPalette systemPalette_;
    bool forced_ = false;
    bool applying_ = false;
};

#endif  // DATAGREP_APPEARANCE_HPP
