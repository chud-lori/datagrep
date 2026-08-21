// Theme.hpp — startup appearance: Fusion style, explicit light palette,
// structural stylesheet (:/style/datagrep.qss). Packaged builds resolve no
// desktop platform theme, so the look must be deliberate, not fallback.

#ifndef DATAGREP_THEME_HPP
#define DATAGREP_THEME_HPP

class QApplication;

namespace dg {

void applyTheme(QApplication& app);

}  // namespace dg

#endif  // DATAGREP_THEME_HPP
