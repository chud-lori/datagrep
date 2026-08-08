import AppKit
import SwiftUI

// The visible half of UpdateCheck.swift: a quiet, dismissible card when a
// newer release exists, and a settings row for the opt-out.
//
// Redraw discipline (P19 / "zero redraws when idle"): nothing here animates,
// nothing spins, nothing polls. The view renders exactly twice in the common
// case — once as nothing, once more if a newer version arrived — and then
// never again until the user clicks it away.

/// Self-driving update notice. Drop it into the window once, e.g. as an
/// overlay on the workbench:
///
///     .overlay(alignment: .bottomTrailing) { UpdateNoticeView() }
///
/// It triggers the once-per-launch check on first appearance (re-appearing is
/// a no-op — the check is guarded in UpdateCheck), renders nothing while
/// there is no update, and shows a small dismissible card when there is one.
struct UpdateNoticeView: View {
    @ObservedObject private var check = UpdateCheck.shared

    var body: some View {
        Group {
            if let manifest = check.available {
                card(manifest)
            }
        }
        .onAppear { check.checkOnLaunchIfEnabled() }
    }

    private func card(_ manifest: UpdateManifest) -> some View {
        HStack(spacing: 8) {
            Image(systemName: "arrow.down.circle")
                .foregroundStyle(Color.accentColor)
            Text("datagrep \(UpdateCheck.normalize(manifest.version)) is available")
                .font(.callout)
                .lineLimit(1)
                .fixedSize()
            Button("View release") { open(manifest) }
                .buttonStyle(.link)
                .font(.callout)
                .fixedSize()
            Menu {
                Button("Skip this version") { check.skip(manifest) }
                Button("Turn off update checks") {
                    UpdatePrefs.checkOnLaunch = false
                    check.dismiss()
                }
            } label: {
                Image(systemName: "ellipsis.circle")
                    .foregroundStyle(.secondary)
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()
            .help("Skip this version, or turn update checks off")
            Button {
                check.dismiss()
            } label: {
                Image(systemName: "xmark")
                    .foregroundStyle(.secondary)
            }
            .buttonStyle(.borderless)
            .help("Dismiss (until the next launch)")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 1)
        )
        .shadow(color: .black.opacity(0.18), radius: 12, y: 4)
        .padding(16)
    }

    private func open(_ manifest: UpdateManifest) {
        let url =
            manifest.releaseURL
            ?? URL(string: "https://github.com/chud-lori/datagrep/releases")!
        NSWorkspace.shared.open(url)
    }
}

/// The opt-out, for whatever settings surface the app grows. The wording is
/// the whole point: say exactly what the check does and sends, no more.
struct UpdateSettingsView: View {
    @State private var enabled = UpdatePrefs.checkOnLaunch

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Toggle("Check for updates at launch", isOn: $enabled)
                .onChange(of: enabled) { _, newValue in
                    UpdatePrefs.checkOnLaunch = newValue
                }
            Text(
                """
                One GET of a static JSON file \
                (chud-lori.github.io/datagrep/latest.json), once per launch, \
                only to compare version numbers. No telemetry, no identifiers, \
                nothing about you or your databases. Updates are never \
                downloaded or installed automatically. Turn this off for zero \
                outbound traffic.
                """
            )
            .font(.caption)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
        }
    }
}
