// Theme.hpp — startup appearance: Fusion style, explicit light palette,
// structural stylesheet (:/style/datagrep.qss). Packaged builds resolve no
// desktop platform theme, so the look must be deliberate, not fallback.

#ifndef DATAGREP_THEME_HPP
#define DATAGREP_THEME_HPP

#include <QPalette>

namespace dg {

// The two palettes Appearance switches between; light is also the startup look.
QPalette lightPalette();
QPalette darkPalette();

// Re-applies :/style/datagrep.qss with the light or dark color table.
void applyStyleSheet(bool dark);

void applyTheme();

}  // namespace dg

#endif  // DATAGREP_THEME_HPP
